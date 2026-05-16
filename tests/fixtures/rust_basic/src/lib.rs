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
