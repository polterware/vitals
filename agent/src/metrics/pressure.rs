/// The `some` line's avg10 of a `/proc/pressure/*` file.
pub fn parse_psi_avg10(text: &str) -> Option<f64> {
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("some") {
            continue;
        }
        for field in line.split_whitespace() {
            if let Some(value) = field.strip_prefix("avg10=") {
                return value.parse().ok();
            }
        }
    }
    None
}
