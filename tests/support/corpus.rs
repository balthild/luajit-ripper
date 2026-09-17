//! A directory of dumps, and sampling it.
//!
//! A corpus is a local dataset that is never committed, so a checkout has none
//! and the harnesses over one do nothing. Nothing here knows anything about a
//! particular corpus, so any collection of `.ljbc` files works:
//!
//! * `LJR_CORPUS=<dir>` points at the directory. Without it a harness skips.
//! * `LJR_SAMPLE=<n>` checks `n` of the dumps, and `LJR_SAMPLE=0` or
//!   `LJR_SAMPLE=full` checks all of them.
//! * `LJR_SEED=<n>` pins which ones are drawn, for repeating a run.

use std::path::{Path, PathBuf};

use super::rng::Rng;

/// How many dumps a run over the corpus looks at by default.
///
/// The corpus can be tens of thousands of files and the corpus harnesses take
/// minutes over all of them, which is too slow to sit through after every
/// change. They check a random sample instead, and the full set is only looked
/// at when it is asked for.
const CORPUS_SAMPLE: usize = 128;

/// As much of the corpus as this run should look at.
#[derive(Debug)]
pub struct Corpus {
    /// Directory the dumps live in.
    pub dir: PathBuf,
    /// The dumps to check: a sample, unless the whole set was asked for.
    pub files: Vec<PathBuf>,
    /// How many dumps the corpus holds in total.
    pub total: usize,
}

impl Corpus {
    /// Loads the corpus, or reports that there is none and gives back `None`.
    ///
    /// A test that gets `None` is meant to return, which is what happens on a
    /// checkout without the corpus.
    pub fn load() -> Option<Corpus> {
        let Some(dir) = corpus_dir() else {
            eprintln!("corpus not present, skipping");
            return None;
        };

        let all = dumps_in(&dir);
        let total = all.len();
        if total == 0 {
            eprintln!("corpus directory {} is empty, skipping", dir.display());
            return None;
        }

        let sample_size = sample_size();
        let seed = seed();
        let files = sample(&all, sample_size, &mut Rng::new(seed));
        if files.len() == total {
            eprintln!("corpus: all {total} dumps");
        } else {
            eprintln!(
                "corpus: {} of {total} dumps sampled with seed {seed} \
                 (LJR_SAMPLE=<n> changes the count, LJR_SEED=<n> the selection)",
                files.len()
            );
        }

        Some(Corpus { dir, files, total })
    }
}

// MARK: location

/// The directory the corpus lives in, if there is one.
///
/// The corpus is a local dataset that is never committed, so there is nothing
/// to fall back on when `LJR_CORPUS` is unset: a harness that finds no corpus
/// does nothing.
pub fn corpus_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LJR_CORPUS")?);
    dir.is_dir().then_some(dir)
}

/// Every dump below `dir`, in path order.
fn dumps_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("the corpus directory should be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "ljbc"))
        .collect();
    files.sort();
    files
}

// MARK: sampling

/// How many dumps to look at, where `0` means all of them.
fn sample_size() -> usize {
    match std::env::var("LJR_SAMPLE") {
        Ok(value) if is_full(&value) => 0,
        Ok(value) => value.parse().unwrap_or(CORPUS_SAMPLE),
        Err(_) => CORPUS_SAMPLE,
    }
}

/// Whether a sample size asks for the whole corpus.
fn is_full(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "full")
}

/// The seed this run samples with.
///
/// `LJR_SEED=<n>` makes a run repeatable. Without one the clock is used, so two
/// runs look at different dumps; the generator stirs its state enough for
/// seeds a nanosecond apart to land far from each other.
fn seed() -> u64 {
    match std::env::var("LJR_SEED") {
        Ok(value) => value.trim().parse().unwrap_or_else(|_| time_seed()),
        Err(_) => time_seed(),
    }
}

fn time_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0x9e37_79b9_7f4a_7c15, |elapsed| elapsed.as_nanos() as u64)
}

/// Picks `size` dumps out of the list, in path order.
///
/// The names are content hashes, so any few of them are as arbitrary as any
/// other few. Drawing them at random rather than off a fixed stride is what
/// keeps a partial run from looking at the same dumps every time, and the seed
/// is what makes a run repeatable when one of them has to be looked at again.
fn sample(files: &[PathBuf], size: usize, rng: &mut Rng) -> Vec<PathBuf> {
    if size == 0 || size >= files.len() {
        return files.to_vec();
    }

    // Partial Fisher-Yates: the first `size` entries of the shuffled indices
    // are the sample, and no dump can be drawn twice.
    let mut indices: Vec<usize> = (0..files.len()).collect();
    for position in 0..size {
        let swap = position + rng.below(files.len() - position);
        indices.swap(position, swap);
    }
    indices.truncate(size);

    let mut picked: Vec<PathBuf> = indices
        .into_iter()
        .map(|index| files[index].clone())
        .collect();
    picked.sort();
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(count: usize) -> Vec<PathBuf> {
        (0..count)
            .map(|index| PathBuf::from(format!("{index}.ljbc")))
            .collect()
    }

    #[test]
    fn a_sample_is_the_size_that_was_asked_for() {
        let files = numbered(1000);
        for size in [1, 4, 128, 999] {
            assert_eq!(sample(&files, size, &mut Rng::new(1)).len(), size);
        }
    }

    #[test]
    fn a_sample_of_everything_is_everything() {
        let files = numbered(10);
        let mut rng = Rng::new(7);
        assert_eq!(sample(&files, 10, &mut rng), files);
        assert_eq!(sample(&files, 0, &mut rng), files);
        assert_eq!(sample(&files, 99, &mut rng), files);
    }

    #[test]
    fn a_sample_is_a_subset_of_the_corpus() {
        let files = numbered(1000);
        let picked = sample(&files, 333, &mut Rng::new(42));
        // No dump is drawn twice...
        let mut unique = picked.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), picked.len(), "a dump was drawn twice");
        // ...every one of them comes from the corpus...
        assert!(picked.iter().all(|path| files.contains(path)));
        // ...and they are handed over in path order.
        assert_eq!(picked, unique);
    }

    #[test]
    fn a_seed_pins_the_sample() {
        let files = numbered(1000);
        let picked = |seed| sample(&files, 64, &mut Rng::new(seed));
        assert_eq!(picked(2026), picked(2026));
        assert_ne!(picked(2026), picked(2027));
    }

    #[test]
    fn only_zero_and_full_ask_for_the_whole_corpus() {
        assert!(is_full("0"));
        assert!(is_full("full"));
        assert!(is_full(" FULL "));
        assert!(!is_full("1"));
        assert!(!is_full("128"));
        assert!(!is_full(""));
    }
}
