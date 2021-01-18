#[must_use]
pub struct RunOnDrop(Option<Box<dyn FnOnce() + Send>>);

impl RunOnDrop {
    pub fn new<F: FnOnce() + Send + 'static>(func: F) -> RunOnDrop {
        RunOnDrop(Some(Box::new(func)))
    }
}

impl Drop for RunOnDrop {
    fn drop(&mut self) {
        if let Some(func) = self.0.take() {
            func();
        }
    }
}
