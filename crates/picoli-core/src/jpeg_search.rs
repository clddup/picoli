//! Avoid repeated real JPEG trials without retaining every encoded file.
//! Search boundaries and both historical size/ID tie rules remain unchanged.
use crate::{Error, analysis::Score, optimize::Candidate, quality::Strength};

struct Trial {
    scale: u16,
    accepted: bool,
    bytes: usize,
    id: String,
}
impl Trial {
    fn record(scale: u16, candidate: &Candidate, accepts: impl FnOnce(&Score) -> bool) -> Self {
        Self {
            scale,
            accepted: accepts(&candidate.score),
            bytes: candidate.bytes,
            id: candidate.id.clone(),
        }
    }
}

pub(crate) fn run(
    strength: Strength,
    mut evaluate: impl FnMut(u16) -> Result<Candidate, Error>,
) -> Result<Option<Candidate>, Error> {
    let limits = strength.limits();
    let finest = evaluate(1)?;
    if !limits.accepts(&finest.score) {
        return Ok(None);
    }
    let mut trials = vec![Trial::record(1, &finest, |s| limits.accepts(s))];
    let (mut low, mut high, mut boundary) = (1i32, 5000i32, 1i32);
    let mut result = finest;
    while low <= high {
        let scale = (low + high) / 2;
        let accepted = if let Some(old) = trials.iter().find(|t| t.scale == scale as u16) {
            old.accepted
        } else {
            let candidate = evaluate(scale as u16)?;
            let trial = Trial::record(scale as u16, &candidate, |s| limits.accepts(s));
            let accepted = trial.accepted;
            if accepted && candidate.bytes < result.bytes {
                result = candidate;
            }
            trials.push(trial);
            accepted
        };
        if accepted {
            boundary = scale;
            low = scale + 1;
        } else {
            high = scale - 1;
        }
    }
    for scale in (boundary - 4).max(1)..=(boundary + 4).min(5000) {
        if let Some(old) = trials.iter().find(|t| t.scale == scale as u16) {
            // Binary selection deliberately uses strict size, whereas the old
            // neighborhood pass also breaks equal-size ties by candidate ID.
            // Recreate a cached candidate only if that exact old tie rule needs
            // its bytes. Never broaden the old search's tie-breaking domain.
            if old.accepted && (old.bytes, &old.id) < (result.bytes, &result.id) {
                result = evaluate(scale as u16)?;
            }
        } else {
            let candidate = evaluate(scale as u16)?;
            if limits.accepts(&candidate.score)
                && (candidate.bytes, &candidate.id) < (result.bytes, &result.id)
            {
                result = candidate;
            }
        }
    }
    Ok(Some(result))
}
