use objc2::runtime::{ClassBuilder, AnyObject};
use objc2::class;

fn test() {
    let _b = ClassBuilder::new(c"TestClass", class!(NSObject)).unwrap();
}
