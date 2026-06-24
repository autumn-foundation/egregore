// A comment containing a decoy call: free_function(10);

pub fn free_function(x: i32) -> i32 {
    x + 1
}

pub struct MyStruct<T> {
    pub value: T,
}

impl<T> MyStruct<T> {
    pub fn new(value: T) -> Self {
        Self { value }
    }
    pub fn get_value(&self) -> &T {
        &self.value
    }
}

pub trait MyTrait {
    fn trait_method(&self);
}

impl MyTrait for MyStruct<i32> {
    fn trait_method(&self) {
        free_function(42);
    }
}

pub mod nested_mod {
    pub fn nested_function() {}
}

pub use nested_mod::nested_function as aliased_func;

use std::collections::HashMap;

macro_rules! my_macro {
    ($name:ident) => {
        fn $name() {}
    };
}
my_macro!(macro_function);

pub const DECOY_STR: &str = "free_function(20);";
