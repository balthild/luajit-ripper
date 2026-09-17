//! A seeded generator, for drawing a sample.
//!
//! Picking a few dumps out of a corpus is not a security decision, so a small
//! generator with no state to speak of is enough. SplitMix64 is the obvious fit:
//! each step adds a constant and then avalanches it, which is what keeps
//! neighbouring seeds unrelated.

/// A SplitMix64 generator.
///
/// The sequence is fixed by the seed, so a run that reports its seed can be
/// repeated.
pub struct Rng(u64);

impl Rng {
    /// A generator whose sequence is the one `seed` names.
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    /// The next value of the sequence.
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    /// A value below `bound`, which has to be positive.
    pub fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first values a seed produces.
    fn sequence(seed: u64, count: usize) -> Vec<u64> {
        let mut rng = Rng::new(seed);
        (0..count).map(|_| rng.next()).collect()
    }

    #[test]
    fn a_seed_pins_the_sequence() {
        assert_eq!(sequence(2026, 8), sequence(2026, 8));
        assert_ne!(sequence(2026, 8), sequence(2027, 8));
    }

    #[test]
    fn a_step_in_the_seed_changes_most_of_the_bits() {
        // The default seed is a clock reading, so two runs a nanosecond apart
        // have to land somewhere else entirely. A state that only counted would
        // hand back values that differ by one, which is exactly the failure this
        // guards against, so what is looked at is how much of the output moves
        // when the seed moves.
        let mut flipped = 0u32;
        let mut pairs = 0u32;
        for seed in 0..64u64 {
            let before = Rng::new(seed).next();
            let after = Rng::new(seed + 1).next();
            flipped += (before ^ after).count_ones();
            pairs += 1;
        }

        let average = flipped / pairs;
        assert!(
            average > 24,
            "nearby seeds differ in only {average} of 64 bits on average"
        );
    }

    #[test]
    fn successive_values_are_not_evenly_spaced() {
        // Same idea one step further in: a counter, or anything equally weak,
        // steps by the same amount every time.
        let values = sequence(1, 32);
        let steps: Vec<u64> = values
            .windows(2)
            .map(|pair| pair[1].wrapping_sub(pair[0]))
            .collect();
        let first = steps[0];
        assert!(
            steps.iter().any(|step| *step != first),
            "every step of the sequence was the same"
        );
    }

    #[test]
    fn a_value_stays_below_the_bound_it_was_given() {
        let mut rng = Rng::new(7);
        for bound in [1, 2, 3, 1000] {
            for _ in 0..100 {
                assert!(rng.below(bound) < bound, "bound {bound}");
            }
        }
    }

    #[test]
    fn a_short_sequence_does_not_repeat_a_value() {
        // Not something a generator this size promises in general, but a repeat
        // inside a short run would mean the state barely moves.
        let values = sequence(42, 64);
        let mut unique = values.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), values.len(), "a value came up twice");
    }
}
