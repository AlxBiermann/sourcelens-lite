/// A counter that wraps around at a limit.
pub struct Counter {
    value: u32,
    limit: u32,
}

impl Counter {
    fn new(limit: u32) -> Self {
        Self { value: 0, limit }
    }

    fn increment(&mut self) {
        self.value = (self.value + 1) % self.limit;
    }

    fn value(&self) -> u32 {
        self.value
    }
}
