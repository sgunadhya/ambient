use cozo::Db;
use cozo::ScriptMutability;
use std::collections::BTreeMap;

fn main() {
    let db = Db::new("mem", "", Default::default()).unwrap();
    db.run_script(
        ":create notes { id: String => title: String, content: String }",
        BTreeMap::new(),
        ScriptMutability::Mutable,
    )
    .unwrap();

    let syntaxes = [
        r#"::fts create notes:fts { fields: [title, content] }"#,
        r#"::fts create notes:fts { extractor: [title, content] }"#,
        r#"::fts create notes:fts { index: [title, content] }"#,
    ];

    for s in syntaxes {
        println!("Trying: {}", s);
        match db.run_script(s, BTreeMap::new(), ScriptMutability::Mutable) {
            Ok(_) => {
                println!("SUCCESS!");
                break;
            }
            Err(e) => println!("ERROR: {}", e),
        }
    }
}
