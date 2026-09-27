use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

pub fn start(message: &str, length: Option<u64>) -> ProgressBar {
    let bar = match length {
        Some(length) => ProgressBar::new(length.max(1)),
        None => ProgressBar::new_spinner(),
    };
    let style = ProgressStyle::with_template(
        "{spinner:.green} {msg} [{bar:30.cyan/blue}] {pos}/{len} {elapsed_precise} ETA {eta}",
    )
    .unwrap_or_else(|_| ProgressStyle::default_spinner())
    .progress_chars("=> ");
    bar.set_style(style);
    bar.set_message(message.to_owned());
    bar.enable_steady_tick(Duration::from_millis(100));
    bar
}

pub fn phase(is_warmup: bool, index: usize, warmup: usize, iterations: usize) -> String {
    if is_warmup {
        format!("warmup {}/{}", index + 1, warmup)
    } else {
        format!("run {}/{}", index + 1, iterations)
    }
}
