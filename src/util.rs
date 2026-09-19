//! Small helpers: edit distance/spell checking and time.

use std::time::{SystemTime, UNIX_EPOCH};

/// Levenshtein distance between `a` and `b`, bailing out early once the
/// distance is known to exceed `max_edit_distance` (0 disables the cutoff).
///
/// Matches ninja's `EditDistance`, so its spelling suggestions agree.
pub fn edit_distance(a: &str, b: &str, allow_replacements: bool, max_edit_distance: usize) -> usize {
    let s1 = a.as_bytes();
    let s2 = b.as_bytes();
    let m = s1.len();
    let n = s2.len();

    let mut previous: Vec<usize> = (0..=n).collect();
    let mut current: Vec<usize> = vec![0; n + 1];

    for y in 1..=m {
        current[0] = y;
        let mut best_this_row = current[0];
        for x in 1..=n {
            current[x] = if allow_replacements {
                let sub = previous[x - 1] + usize::from(s1[y - 1] != s2[x - 1]);
                sub.min(current[x - 1].min(previous[x]) + 1)
            } else if s1[y - 1] == s2[x - 1] {
                previous[x - 1]
            } else {
                current[x - 1].min(previous[x]) + 1
            };
            best_this_row = best_this_row.min(current[x]);
        }
        if max_edit_distance != 0 && best_this_row > max_edit_distance {
            return max_edit_distance + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[n]
}

/// Return the entry of `words` closest to `text`, if one is close enough.
pub fn spellcheck<'a>(text: &str, words: &[&'a str]) -> Option<&'a str> {
    const MAX_VALID_EDIT_DISTANCE: usize = 3;
    let mut min_distance = MAX_VALID_EDIT_DISTANCE + 1;
    let mut result = None;
    for w in words {
        let d = edit_distance(w, text, true, MAX_VALID_EDIT_DISTANCE);
        if d < min_distance {
            min_distance = d;
            result = Some(*w);
        }
    }
    result
}

/// Milliseconds since the Unix epoch, used for build timing.
pub fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

/// Format an mtime (nanoseconds since the Unix epoch) for diagnostics.
pub fn format_mtime(mtime: i64) -> String {
    mtime.to_string()
}

/// The number of usable CPUs.
pub fn processor_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// ninja's default for `-j`: a couple more jobs than there are CPUs, so the
/// pipeline stays full while some jobs block on I/O.
pub fn guess_parallelism() -> usize {
    match processor_count() {
        0 | 1 => 2,
        2 => 3,
        n => n + 2,
    }
}

/// The system's 1-minute load average, if it can be determined.
///
/// Implemented on Linux by reading `/proc/loadavg`; returns `None` elsewhere,
/// which disables `-l`.
pub fn load_average() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/loadavg").ok()?;
        text.split_whitespace().next()?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(edit_distance("", "", true, 0), 0);
        assert_eq!(edit_distance("", "a", true, 0), 1);
        assert_eq!(edit_distance("kitten", "sitting", true, 0), 3);
        assert_eq!(edit_distance("kitten", "sitting", false, 0), 5);
        // Early cutoff.
        assert_eq!(edit_distance("abcdefg", "xyz", true, 2), 3);
    }

    #[test]
    fn suggestions() {
        assert_eq!(spellcheck("targts", &["targets", "rules"]), Some("targets"));
        assert_eq!(spellcheck("zzzzzzzz", &["targets", "rules"]), None);
    }
}
