//! One number that says which frames a run actually saw, in which order.
//!
//! `--motion settle` once produced a file byte-identical to `--motion scroll`,
//! fingerprint 80369874 for both, and every settling number computed from it
//! looked entirely plausible: five stops, some settling in twenty frames, some
//! never. The cause was that the export read the source's "screen did not
//! change" as "nothing yet, ask again" and looped, so only the moving frames
//! reached the file — and the moving frames of `settle` *are* `scroll`.
//!
//! It was caught because a bytes-per-frame figure happened to match another run
//! exactly. That is not a method, that is luck, and it is the second time this
//! project has been rescued by two numbers coinciding when they should not
//! have.
//!
//! This makes the coincidence into a value. The chain folds the pixels of every
//! frame that carried content **and one marker byte for every poll that found
//! the screen unchanged**. The marker is the whole point: a quiet frame is
//! invisible in the pixels, so a hash over pixels alone cannot tell a run that
//! held still from a run that never stopped. With it, `settle` and `scroll`
//! differ in the first cycle.
//!
//! **Sampled, not complete.** Folding two megabytes a frame would put a
//! millisecond of hashing inside a loop measuring milliseconds. Every 1021st
//! byte is taken instead — a prime stride, about two thousand samples per 1080p
//! frame, a couple of microseconds. That is enough to tell different content
//! apart and is not enough to resist someone constructing a collision. Nobody
//! is attacking this; the failures it exists to catch are structural.

/// Stride through the frame, in bytes. Prime, so it does not line up with a row
/// or a pixel and sample the same colour channel every time.
const STRIDE: usize = 1021;

/// Folded once per poll that found nothing. Any value would do; what matters is
/// that a quiet poll leaves a trace at all.
const QUIET: u8 = 0xA5;

#[derive(Debug, Clone)]
pub struct Chain {
    hash: u64,
    polls: u64,
    content: u64,
    quiet: u64,
}

impl Default for Chain {
    fn default() -> Self {
        Self::new()
    }
}

impl Chain {
    pub fn new() -> Self {
        // FNV-1a, the same constants the screenshots and both writers use, so
        // the figures are recognisably the same kind of claim.
        Self { hash: 0xcbf2_9ce4_8422_2325, polls: 0, content: 0, quiet: 0 }
    }

    fn fold(&mut self, byte: u8) {
        self.hash ^= u64::from(byte);
        self.hash = self.hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    /// A poll that returned a frame.
    pub fn content(&mut self, pixels: &[u8]) {
        // The length goes in first. Two frames of different size that happen to
        // sample the same bytes are not the same frame.
        for b in (pixels.len() as u64).to_le_bytes() {
            self.fold(b);
        }
        let mut i = 0;
        while i < pixels.len() {
            self.fold(pixels[i]);
            i += STRIDE;
        }
        self.polls += 1;
        self.content += 1;
    }

    /// A poll that found the screen unchanged.
    pub fn quiet(&mut self) {
        self.fold(QUIET);
        self.polls += 1;
        self.quiet += 1;
    }

    pub fn polls(&self) -> u64 {
        self.polls
    }

    pub fn content_frames(&self) -> u64 {
        self.content
    }

    pub fn quiet_frames(&self) -> u64 {
        self.quiet
    }

    pub fn short(&self) -> String {
        format!("{:016x}", self.hash)
    }

    /// The line every mode prints, so two runs can be compared at a glance.
    pub fn render(&self) -> String {
        format!(
            "  опрошено {} · с содержимым {} · экран не менялся {} · цепочка {}",
            self.polls(),
            self.content_frames(),
            self.quiet_frames(),
            self.short()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seed: u8, n: usize) -> Vec<u8> {
        (0..n).map(|i| seed.wrapping_add((i % 251) as u8)).collect()
    }

    /// The exact collapse this exists to prevent: throw the quiet polls away and
    /// two different runs become one.
    #[test]
    fn a_quiet_poll_changes_the_chain() {
        let f = frame(7, 8192);

        let mut with = Chain::new();
        with.content(&f);
        with.quiet();
        with.content(&f);

        let mut without = Chain::new();
        without.content(&f);
        without.content(&f);

        assert_ne!(with.short(), without.short(), "тихий опрос обязан оставлять след");
        assert_eq!(with.polls(), 3);
        assert_eq!(without.polls(), 2);
    }

    /// Order matters: the same frames held at different moments are a different
    /// recording.
    #[test]
    fn the_same_frames_in_a_different_rhythm_differ() {
        let f = frame(3, 8192);

        let mut a = Chain::new();
        a.content(&f);
        a.quiet();
        a.quiet();
        a.content(&f);

        let mut b = Chain::new();
        a.short();
        b.content(&f);
        b.content(&f);
        b.quiet();
        b.quiet();

        assert_ne!(a.short(), b.short());
        assert_eq!(a.polls(), b.polls(), "число опросов одинаково — различает только порядок");
    }

    #[test]
    fn the_same_run_twice_gives_the_same_chain() {
        let build = || {
            let mut c = Chain::new();
            for k in 0..5u8 {
                c.content(&frame(k, 4096));
                c.quiet();
            }
            c
        };
        assert_eq!(build().short(), build().short());
    }

    /// Different pixels have to move it, or it is only counting.
    #[test]
    fn different_content_gives_a_different_chain() {
        let mut a = Chain::new();
        a.content(&frame(1, 8192));
        let mut b = Chain::new();
        b.content(&frame(2, 8192));
        assert_ne!(a.short(), b.short());
    }

    /// Sampling must not make two frames of different length look alike.
    #[test]
    fn a_different_frame_size_moves_the_chain_even_when_samples_agree() {
        let long = frame(9, 8192);
        let short = long[..4096].to_vec();
        let mut a = Chain::new();
        a.content(&long);
        let mut b = Chain::new();
        b.content(&short);
        assert_ne!(a.short(), b.short());
    }
}
