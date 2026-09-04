pub mod equiv;
pub mod errors;
pub mod cli;
pub mod finalizer;
pub mod gitio;
pub mod jsonio;
pub mod planner;
pub mod refs;
pub mod scheduler;
pub mod state;

#[cfg(test)]
mod scaffold_tests {
    #[test]
    fn it_builds() {
        assert!(true);
    }
}
