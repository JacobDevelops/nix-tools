mod model;
mod session;
mod view;

pub use session::{DisplayContext, OutputMode, UiSession};

#[cfg(test)]
#[path = "model_test.rs"]
mod model_test;

#[cfg(test)]
#[path = "session_test.rs"]
mod session_test;

#[cfg(test)]
#[path = "view_test.rs"]
mod view_test;
