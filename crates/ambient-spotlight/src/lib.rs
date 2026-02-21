use std::collections::HashMap;
use std::sync::{mpsc, Mutex};
use std::thread;

use ambient_core::{CoreError, KnowledgeUnit, RawEvent, Result, SourceId, SpotlightExporter};
use objc2::rc::autoreleasepool;
use uuid::Uuid;

pub struct SpotlightAdapter;

impl SpotlightAdapter {
    pub fn source_id(&self) -> SourceId {
        SourceId::new("spotlight")
    }

    pub fn watch(&self, tx: mpsc::Sender<RawEvent>) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            thread::Builder::new()
                .name("spotlight-adapter".to_string())
                .spawn(move || {
                    let _ = macos_bridge::run_spotlight_query_notifications(tx);
                })
                .map_err(|e| {
                    CoreError::Internal(format!("failed to spawn spotlight adapter thread: {e}"))
                })?;
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = tx;
        }

        Ok(())
    }
}

#[derive(Default)]
pub struct CoreSpotlightExporter {
    index: Mutex<HashMap<Uuid, SpotlightDocument>>,
}

#[derive(Debug, Clone)]
pub struct SpotlightDocument {
    pub title: Option<String>,
    pub description: String,
    pub keywords: Vec<String>,
    pub content_url: String,
}

impl CoreSpotlightExporter {
    fn to_document(unit: &KnowledgeUnit) -> SpotlightDocument {
        let summary: String = unit.content.chars().take(200).collect();
        let keywords = unit
            .metadata
            .get("concepts")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();

        SpotlightDocument {
            title: unit.title.clone(),
            description: summary,
            keywords,
            content_url: format!("ambient://unit/{}", unit.id),
        }
    }
}

impl SpotlightExporter for CoreSpotlightExporter {
    fn export(&self, unit: &KnowledgeUnit) -> Result<()> {
        autoreleasepool(|_| {
            let doc = Self::to_document(unit);
            self.index
                .lock()
                .map_err(|_| CoreError::Internal("spotlight index lock poisoned".to_string()))?
                .insert(unit.id, doc.clone());

            #[cfg(target_os = "macos")]
            {
                if let Err(e) = macos_bridge::index_document(unit.id, &doc) {
                    eprintln!("spotlight export failed for {}: {e}", unit.id);
                }
            }

            Ok(())
        })
    }

    fn delete(&self, id: Uuid) -> Result<()> {
        autoreleasepool(|_| {
            self.index
                .lock()
                .map_err(|_| CoreError::Internal("spotlight index lock poisoned".to_string()))?
                .remove(&id);

            #[cfg(target_os = "macos")]
            {
                if let Err(e) = macos_bridge::delete_document(id) {
                    eprintln!("spotlight delete failed for {}: {e}", id);
                }
            }

            Ok(())
        })
    }
}

#[cfg(target_os = "macos")]
mod macos_bridge {
    use std::collections::HashSet;
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::ptr;
    use std::sync::{mpsc, Arc, Mutex};

    use ambient_core::{CoreError, RawEvent, RawPayload, Result, SourceId};
    use block2::RcBlock;
    use chrono::{DateTime, Utc};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use uuid::Uuid;

    use crate::SpotlightDocument;

    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}
    #[link(name = "CoreSpotlight", kind = "framework")]
    unsafe extern "C" {}

    pub fn run_spotlight_query_notifications(tx: mpsc::Sender<RawEvent>) -> Result<()> {
        objc2::rc::autoreleasepool(|_| {
            let query: *mut AnyObject = unsafe { msg_send![class!(NSMetadataQuery), new] };
            if query.is_null() {
                return Err(CoreError::Internal(
                    "NSMetadataQuery init failed".to_string(),
                ));
            }

            let pattern = nsstring("kMDItemTextContent != NULL");
            let predicate: *mut AnyObject =
                unsafe { msg_send![class!(NSPredicate), predicateWithFormat: pattern] };
            if !predicate.is_null() {
                let _: () = unsafe { msg_send![query, setPredicate: predicate] };
            }

            let center: *mut AnyObject =
                unsafe { msg_send![class!(NSNotificationCenter), defaultCenter] };
            if center.is_null() {
                return Err(CoreError::Internal(
                    "failed to get NSNotificationCenter".to_string(),
                ));
            }

            let seen: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
            let update_name = nsstring("NSMetadataQueryDidUpdateNotification");
            let finish_name = nsstring("NSMetadataQueryDidFinishGatheringNotification");

            let update_tx = tx.clone();
            let update_seen = Arc::clone(&seen);
            let update_query = query;
            let update_block = RcBlock::new(move |_notification: *mut AnyObject| {
                let _ = emit_results(update_query, &update_tx, &update_seen);
            });

            let finish_tx = tx;
            let finish_seen = Arc::clone(&seen);
            let finish_query = query;
            let finish_block = RcBlock::new(move |_notification: *mut AnyObject| {
                let _ = emit_results(finish_query, &finish_tx, &finish_seen);
            });

            let _update_observer: *mut AnyObject = unsafe {
                msg_send![center,
                    addObserverForName: update_name,
                    object: query,
                    queue: ptr::null_mut::<AnyObject>(),
                    usingBlock: &*update_block
                ]
            };
            let _finish_observer: *mut AnyObject = unsafe {
                msg_send![center,
                    addObserverForName: finish_name,
                    object: query,
                    queue: ptr::null_mut::<AnyObject>(),
                    usingBlock: &*finish_block
                ]
            };

            let started: bool = unsafe { msg_send![query, startQuery] };
            if !started {
                return Err(CoreError::Internal(
                    "failed to start NSMetadataQuery".to_string(),
                ));
            }

            // Keep blocks alive for callback lifetime.
            std::mem::forget(update_block);
            std::mem::forget(finish_block);

            loop {
                let run_loop: *mut AnyObject =
                    unsafe { msg_send![class!(NSRunLoop), currentRunLoop] };
                if run_loop.is_null() {
                    break;
                }
                let until_date: *mut AnyObject =
                    unsafe { msg_send![class!(NSDate), dateWithTimeIntervalSinceNow: 1.0f64] };
                if until_date.is_null() {
                    break;
                }
                let _: () = unsafe { msg_send![run_loop, runUntilDate: until_date] };
            }

            Ok(())
        })
    }

    fn emit_results(
        query: *mut AnyObject,
        tx: &mpsc::Sender<RawEvent>,
        seen: &Arc<Mutex<HashSet<String>>>,
    ) -> Result<()> {
        let result_count: usize = unsafe { msg_send![query, resultCount] };
        for idx in 0..result_count {
            let item: *mut AnyObject = unsafe { msg_send![query, resultAtIndex: idx] };
            if item.is_null() {
                continue;
            }

            let display_name = metadata_string(item, "kMDItemDisplayName").unwrap_or_default();
            let content_type = metadata_string(item, "kMDItemContentType")
                .unwrap_or_else(|| "public.text".to_string());
            let text_content = metadata_string(item, "kMDItemTextContent").unwrap_or_default();
            if text_content.is_empty() {
                continue;
            }

            let bundle_id = metadata_string(item, "kMDItemCFBundleIdentifier").unwrap_or_default();
            let file_path = metadata_string(item, "kMDItemPath");
            let file_url = file_path.as_ref().map(|path| format!("file://{path}"));

            let modified =
                metadata_date(item, "kMDItemFSContentChangeDate").unwrap_or_else(Utc::now);
            let dedupe_key = format!(
                "{}:{}",
                file_url.clone().unwrap_or_else(|| display_name.clone()),
                modified.timestamp()
            );
            let should_emit = seen
                .lock()
                .map(|mut guard| guard.insert(dedupe_key))
                .unwrap_or(false);
            if !should_emit {
                continue;
            }

            let event = RawEvent {
                source: SourceId::new("spotlight"),
                timestamp: Utc::now(),
                payload: RawPayload::SpotlightItem {
                    bundle_id,
                    display_name,
                    content_type,
                    text_content,
                    last_modified: modified,
                    file_url,
                },
            };

            let _ = tx.send(event);
        }
        Ok(())
    }

    pub fn index_document(id: Uuid, doc: &SpotlightDocument) -> Result<()> {
        let attribute_set: *mut AnyObject =
            unsafe { msg_send![class!(CSSearchableItemAttributeSet), alloc] };
        if attribute_set.is_null() {
            return Err(CoreError::Internal(
                "failed to allocate attribute set".to_string(),
            ));
        }

        let content_type = nsstring("public.text");
        let attribute_set: *mut AnyObject =
            unsafe { msg_send![attribute_set, initWithItemContentType: content_type] };
        if attribute_set.is_null() {
            return Err(CoreError::Internal(
                "failed to init attribute set".to_string(),
            ));
        }

        if let Some(title) = &doc.title {
            let ns_title = nsstring(title);
            let _: () = unsafe { msg_send![attribute_set, setTitle: ns_title] };
        }

        let ns_desc = nsstring(&doc.description);
        let _: () = unsafe { msg_send![attribute_set, setContentDescription: ns_desc] };

        if let Some(first_keyword) = doc.keywords.first() {
            let keyword = nsstring(first_keyword);
            let arr: *mut AnyObject =
                unsafe { msg_send![class!(NSArray), arrayWithObject: keyword] };
            if !arr.is_null() {
                let _: () = unsafe { msg_send![attribute_set, setKeywords: arr] };
            }
        }

        let ns_url_str = nsstring(&doc.content_url);
        let url: *mut AnyObject = unsafe { msg_send![class!(NSURL), URLWithString: ns_url_str] };
        if !url.is_null() {
            let _: () = unsafe { msg_send![attribute_set, setContentURL: url] };
        }

        let identifier = nsstring(&id.to_string());
        let domain = nsstring("ambient.unit");
        let item_alloc: *mut AnyObject = unsafe { msg_send![class!(CSSearchableItem), alloc] };
        if item_alloc.is_null() {
            return Err(CoreError::Internal(
                "failed to allocate searchable item".to_string(),
            ));
        }

        let item: *mut AnyObject = unsafe {
            msg_send![item_alloc, initWithUniqueIdentifier: identifier, domainIdentifier: domain, attributeSet: attribute_set]
        };
        if item.is_null() {
            return Err(CoreError::Internal(
                "failed to initialize searchable item".to_string(),
            ));
        }

        let array: *mut AnyObject = unsafe { msg_send![class!(NSArray), arrayWithObject: item] };
        if array.is_null() {
            return Err(CoreError::Internal(
                "failed to create searchable item array".to_string(),
            ));
        }

        let index: *mut AnyObject =
            unsafe { msg_send![class!(CSSearchableIndex), defaultSearchableIndex] };
        if index.is_null() {
            return Err(CoreError::Internal(
                "failed to get default searchable index".to_string(),
            ));
        }

        let _: () = unsafe {
            msg_send![index, indexSearchableItems: array, completionHandler: ptr::null_mut::<AnyObject>()]
        };

        Ok(())
    }

    pub fn delete_document(id: Uuid) -> Result<()> {
        let identifier = nsstring(&id.to_string());
        let ids: *mut AnyObject =
            unsafe { msg_send![class!(NSArray), arrayWithObject: identifier] };
        if ids.is_null() {
            return Err(CoreError::Internal("failed to create id array".to_string()));
        }

        let index: *mut AnyObject =
            unsafe { msg_send![class!(CSSearchableIndex), defaultSearchableIndex] };
        if index.is_null() {
            return Err(CoreError::Internal(
                "failed to get default searchable index".to_string(),
            ));
        }

        let _: () = unsafe {
            msg_send![index, deleteSearchableItemsWithIdentifiers: ids, completionHandler: ptr::null_mut::<AnyObject>()]
        };

        Ok(())
    }

    fn metadata_string(item: *mut AnyObject, key: &str) -> Option<String> {
        let key = nsstring(key);
        let value: *mut AnyObject = unsafe { msg_send![item, valueForAttribute: key] };
        nsstring_to_string(value)
    }

    fn metadata_date(item: *mut AnyObject, key: &str) -> Option<DateTime<Utc>> {
        let key = nsstring(key);
        let value: *mut AnyObject = unsafe { msg_send![item, valueForAttribute: key] };
        if value.is_null() {
            return None;
        }

        let timestamp: f64 = unsafe { msg_send![value, timeIntervalSince1970] };
        DateTime::<Utc>::from_timestamp(timestamp as i64, 0)
    }

    fn nsstring(input: &str) -> *mut AnyObject {
        let c = match CString::new(input) {
            Ok(v) => v,
            Err(_) => return ptr::null_mut(),
        };
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    fn nsstring_to_string(value: *mut AnyObject) -> Option<String> {
        if value.is_null() {
            return None;
        }
        let utf8: *const c_char = unsafe { msg_send![value, UTF8String] };
        if utf8.is_null() {
            return None;
        }
        Some(
            unsafe { CStr::from_ptr(utf8) }
                .to_string_lossy()
                .to_string(),
        )
    }
}
