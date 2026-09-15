//! Gathering the observations §13's gate judges.
//!
//! [`crate::readiness::evaluate`] decides; this produces what it decides on.
//! The split is the point: a check whose evidence is gathered by the same
//! code that grades it can only ever agree with itself, which is how three
//! separate reviews of this module's siblings found circular comparisons.
//!
//! Every function returns a `Result`, and an observation that could not be
//! made stays an `Err`. `evaluate` fails a check on one — it never skips it —
//! so an endpoint that was unreachable reads as "not verified" rather than
//! as "fine".
//!
//! The gateway reads TIG here even though `architecture.md` §4 gives it no
//! read responsibilities. §13 is why: checks 5, 6 and 9 cannot be answered
//! without reading, and `tig_client`'s `TigReader::Gateway` share exists for
//! exactly this. Choosing work stays the controller's.

use serde_json::Value;
use tig_client::TigReadClient;

/// §13 check 9: the pool player ID returned by confirmed data matches the
/// configured identity.
///
/// **What is verified is that TIG holds a player for this id**, not that the
/// reply repeated it. `get-player-data` takes `player_id` as a parameter, so
/// comparing the echoed id against the configured one would compare a value
/// with itself — the circularity §13.1 rejects for check 2 and a reviewer
/// caught in check 1.
///
/// Existence is not circular. A typo, a mainnet identity against a testnet
/// endpoint, or an account that was retired all produce a reply with no
/// player, and the deployment cannot write until its identity is one TIG
/// actually serves.
///
/// **A missing player is HTTP 200, not 404** — verified against live testnet
/// on 2026-09-15: an unknown id returns `{"player": null, ...}` with a
/// success status. A check written as "did the request succeed" would pass
/// for an identity that does not exist, which is the case this exists to
/// catch.
pub async fn confirmed_pool_player_id(
    reader: &TigReadClient,
    block_id: &str,
    configured: &str,
) -> Result<String, String> {
    let body = reader
        .get_json(&format!(
            "/get-player-data?block_id={block_id}&player_id={configured}"
        ))
        .await
        .map_err(|e| format!("get-player-data failed: {e}"))?;

    match body.get("player") {
        None | Some(Value::Null) => Err(format!(
            "TIG holds no player {configured} on this endpoint; the identity or the \
             endpoint is wrong"
        )),
        Some(player) => player
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "the player TIG returned carries no id".to_string()),
    }
}
