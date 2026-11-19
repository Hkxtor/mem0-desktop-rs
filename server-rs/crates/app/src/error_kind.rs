//! Why an upstream call failed, as a stable code.
//!
//! `memory_operations.reason` already says *which step* produced nothing — `provider_error`,
//! `embed_failed`, `unparsed_answer`. That is not enough to act on: "模型调用失败" covers a
//! quota that will pass, a key that never will, and a model name that is simply wrong, and the
//! operator's next move is different for each. The provider's own message is precise but lives
//! in the application log, where the dashboard cannot read it.
//!
//! This module turns the upstream HTTP status into a small closed set of codes the UI can label
//! and attach advice to. Classification is by status because a status is a contract; matching on
//! provider prose is not — every provider words their 429 differently and some word a 500 like
//! a rejection.
//!
//! Only failures that reach us with a status are classified; everything else stays `None` rather
//! than being guessed at. A wrong kind is worse than no kind: it sends the operator to fix
//! something that is not broken.

/// The key is out of quota right now. Waiting, or adding another key, changes the outcome.
pub const RATE_LIMITED: &str = "rate_limited";
/// The key was refused. Retrying with the same key can never work.
pub const AUTH_FAILED: &str = "auth_failed";
/// The endpoint has no such model — typically a name typed into the configuration page.
pub const MODEL_NOT_FOUND: &str = "model_not_found";
/// The provider is unhealthy. Nothing on this side is wrong.
pub const UPSTREAM_ERROR: &str = "upstream_error";
/// The request never got an answer: DNS, TLS, refused connection, timeout.
pub const NETWORK_ERROR: &str = "network_error";

/// Classify an upstream status.
///
/// `None` means "no status was recorded", which is not the same as "no error": an add that was
/// answered with `{"memory": []}` has no upstream failure at all, and a network failure has one
/// but no status to describe it. The caller knows which of those it has — this function only
/// commits to an answer when the status itself decides it.
pub fn classify(status: Option<u16>) -> Option<&'static str> {
    let status = status?;
    Some(match status {
        429 => RATE_LIMITED,
        // 407 is a proxy asking for credentials; the key never left this machine.
        401 | 403 => AUTH_FAILED,
        // 400 is what a provider answers when the *model name* does not exist (OpenAI and
        // Gemini both do this), and it is also what a malformed request gets. We only send
        // well-formed requests built from the operator's configuration, so a 400 here is far
        // more likely a bad model name than our bug — but it is not certain, so it is only
        // called `model_not_found` when the status is the 404-style one. A bare 400 stays
        // `upstream_error`: it is the provider rejecting our request, and the message in the
        // log is what distinguishes the cause.
        404 => MODEL_NOT_FOUND,
        500..=599 => UPSTREAM_ERROR,
        // Every other 4xx (400, 413, 422, ...) is the provider refusing this request. It is
        // not a transient condition, but it is also not something the operator can fix by
        // changing a key, so it is reported as an upstream error with no advice attached.
        400..=499 => UPSTREAM_ERROR,
        _ => return None,
    })
}

/// The kind for a failure that never produced a status.
///
/// Separate from [`classify`] because the caller has to know it actually was a connection
/// failure: an `add` with no extraction in it *also* has no status, and calling that a network
/// error would be a lie the operator would act on.
pub fn network() -> &'static str {
    NETWORK_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quota_is_recognised() {
        assert_eq!(classify(Some(429)), Some(RATE_LIMITED));
    }

    #[test]
    fn rejected_keys_are_recognised() {
        assert_eq!(classify(Some(401)), Some(AUTH_FAILED));
        assert_eq!(classify(Some(403)), Some(AUTH_FAILED));
    }

    #[test]
    fn a_missing_model_is_recognised() {
        assert_eq!(classify(Some(404)), Some(MODEL_NOT_FOUND));
    }

    #[test]
    fn provider_outages_are_recognised() {
        assert_eq!(classify(Some(500)), Some(UPSTREAM_ERROR));
        assert_eq!(classify(Some(502)), Some(UPSTREAM_ERROR));
        assert_eq!(classify(Some(503)), Some(UPSTREAM_ERROR));
    }

    /// A 400 is the provider refusing *this request*. It is deliberately not reported as a bad
    /// model name even though that is the most common cause: guessing wrong sends the operator
    /// to rename a model that is fine.
    #[test]
    fn a_bad_request_is_an_upstream_error_not_a_guess() {
        assert_eq!(classify(Some(400)), Some(UPSTREAM_ERROR));
        assert_eq!(classify(Some(422)), Some(UPSTREAM_ERROR));
    }

    /// No status means no claim. This is what keeps a `{"memory": []}` answer from being
    /// labelled as a failure, and a connection error from being labelled as a 500.
    #[test]
    fn no_status_means_no_kind() {
        assert_eq!(classify(None), None);
    }

    /// Statuses outside the HTTP error ranges are not upstream failures.
    #[test]
    fn non_error_statuses_are_not_classified() {
        assert_eq!(classify(Some(200)), None);
        assert_eq!(classify(Some(304)), None);
    }

    /// Every kind is a distinct code, so the dashboard can key on them.
    #[test]
    fn the_codes_are_distinct() {
        let mut codes = vec![
            RATE_LIMITED,
            AUTH_FAILED,
            MODEL_NOT_FOUND,
            UPSTREAM_ERROR,
            network(),
        ];
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 5);
    }
}
