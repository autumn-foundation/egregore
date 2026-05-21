pub struct Widget {
    value: usize,
}

impl Widget {
    pub fn new(value: usize) -> Self {
        Self { value }
    }
}

impl Widget {
    pub fn value(&self) -> usize {
        self.value
    }
}
