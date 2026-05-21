/*
shift-only header line 01
shift-only header line 02
shift-only header line 03
shift-only header line 04
shift-only header line 05
shift-only header line 06
shift-only header line 07
shift-only header line 08
shift-only header line 09
shift-only header line 10
shift-only header line 11
shift-only header line 12
shift-only header line 13
shift-only header line 14
shift-only header line 15
shift-only header line 16
shift-only header line 17
shift-only header line 18
shift-only header line 19
shift-only header line 20
shift-only header line 21
shift-only header line 22
shift-only header line 23
shift-only header line 24
shift-only header line 25
shift-only header line 26
shift-only header line 27
shift-only header line 28
shift-only header line 29
shift-only header line 30
shift-only header line 31
shift-only header line 32
shift-only header line 33
shift-only header line 34
shift-only header line 35
shift-only header line 36
shift-only header line 37
shift-only header line 38
*/
use std::fmt::Debug;

macro_rules! local_macro {
    () => {};
}

pub mod nested {
    use super::Debug;

    pub const LIMIT: usize = 7;
    pub static NAME: &str = "basic";
    pub type Alias = usize;

    pub struct Widget {
        pub value: usize,
    }

    pub enum Mode {
        Fast,
        Slow,
    }

    pub trait Runner {
        fn run(&self) -> usize;
    }

    impl Widget {
        pub fn new(value: usize) -> Self {
            Self { value }
        }

        pub fn value(&self) -> usize {
            helper(self.value)
        }
    }

    impl Runner for Widget {
        fn run(&self) -> usize {
            self.value()
        }
    }

    fn helper(value: usize) -> usize {
        value
    }

    pub fn debug_name<T: Debug>(name: T) -> String {
        format!("{name:?}")
    }

    #[test]
    fn widget_runs() {
        assert_eq!(Widget::new(1).run(), 1);
    }
}

pub fn answer() -> usize {
    local_macro!();
    nested::Widget::new(42).run()
}
