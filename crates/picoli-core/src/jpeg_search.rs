//! Avoid repeated real JPEG trials without retaining every encoded file.
//! Search boundaries and both historical size/ID tie rules remain unchanged.
use crate::{Error, optimize::Candidate, quality::Strength};

struct Trial {
    scale: u16,
    // Rejected trials need no invented score, byte count or candidate ID.
    accepted: Option<(usize, String)>,
}
impl Trial {
    fn record(scale: u16, candidate: Option<&Candidate>) -> Self {
        Self {
            scale,
            accepted: candidate.map(|c| (c.bytes, c.id.clone())),
        }
    }
}

#[cfg(test)]
pub(crate) fn run(
    strength: Strength,
    mut evaluate: impl FnMut(u16) -> Result<Candidate, Error>,
) -> Result<Option<Candidate>, Error> {
    run_bounded(strength, |scale| evaluate(scale).map(Some))
}

pub(crate) fn run_bounded(
    strength: Strength,
    mut evaluate: impl FnMut(u16) -> Result<Option<Candidate>, Error>,
) -> Result<Option<Candidate>, Error> {
    let limits = strength.limits();
    let mut evaluate =
        |scale| evaluate(scale).map(|candidate| candidate.filter(|c| limits.accepts(&c.score)));
    let Some(finest) = evaluate(1)? else {
        return Ok(None);
    };
    let mut trials = vec![Trial::record(1, Some(&finest))];
    let (mut low, mut high, mut boundary) = (1i32, 5000i32, 1i32);
    let mut result = finest;
    while low <= high {
        let scale = (low + high) / 2;
        let accepted = if let Some(old) = trials.iter().find(|t| t.scale == scale as u16) {
            old.accepted.is_some()
        } else {
            let candidate = evaluate(scale as u16)?;
            let trial = Trial::record(scale as u16, candidate.as_ref());
            let accepted = candidate.is_some();
            if let Some(candidate) = candidate
                && candidate.bytes < result.bytes
            {
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
            // Binary uses strict size; neighborhood also breaks ties by ID.
            // Only reconstruct a cached candidate when the old rule needs it.
            if let Some((bytes, id)) = &old.accepted
                && (*bytes, id) < (result.bytes, &result.id)
            {
                result = evaluate(scale as u16)?.ok_or_else(|| {
                    Error::Encoding("cached accepted JPEG trial changed admission".into())
                })?;
            }
        } else if let Some(candidate) = evaluate(scale as u16)?
            && (candidate.bytes, &candidate.id) < (result.bytes, &result.id)
        {
            result = candidate;
        }
    }
    Ok(Some(result))
}
