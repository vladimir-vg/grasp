//! The materialized views, saved with a checkpoint so a snapshot survives a
//! restart.
//!
//! `send_snapshot=true` is not answered by the circuit — `dbsp` has no way to
//! read a relation's contents back — but by a fold this server keeps of every
//! delta a materialized view has produced. A circuit restored from a checkpoint
//! replays nothing, so without this file that fold would start empty, and a
//! snapshot would report a view as having no rows while the circuit held them.
//! A client has no way to tell that answer from the truth.
//!
//! **Captured at the same instant as the circuit's state.** The rows are taken
//! on the circuit thread, between transactions, beside `prepare` — the same
//! thread that folds deltas, so no transaction can land between the two. And
//! they are written before the manifest, so a checkpoint whose manifest exists
//! has this file too.
//!
//! **rkyv, not JSON.** The JSON codec is the obvious choice and is lossy in two
//! ways that matter here: a non-finite float encodes as `null`
//! (`tests/invariants.rs`, `non_finite_floats_encode_as_null`), so a NaN row
//! would reload as a different key that no later retraction could cancel; and
//! its decoder reads flat rows only, so an indexed view could not be reloaded at
//! all. rkyv is exact. What it costs is a dependence on `DynValue`'s archived
//! layout, which is exactly what the manifest's format digest checks — so
//! [`load`] is to be called after `Runner::build` has accepted the manifest,
//! never before.

use crate::circuit::Rows;
use grasp_dbsp::value::DynValue;
use rkyv::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// The file, inside the checkpoint's own directory.
pub const FILE: &str = "materialized.bin";

/// Every saved view and its rows, in the form rkyv writes.
type Saved = Vec<(String, Vec<(DynValue, Option<DynValue>, dbsp::ZWeight)>)>;

/// Writes the views' rows into `dir`, fsynced.
///
/// Always written, even with nothing materialized, so that "this checkpoint
/// saved no views" and "this file is missing" are different facts on resume.
pub fn save(dir: &Path, views: &[(String, Arc<Rows>)]) -> Result<(), String> {
    let saved: Saved = views
        .iter()
        .map(|(name, rows)| {
            (
                name.clone(),
                rows.iter()
                    .map(|((key, value), weight)| (key.clone(), value.clone(), *weight))
                    .collect(),
            )
        })
        .collect();
    let bytes = rkyv::to_bytes::<_, 4096>(&saved)
        .map_err(|e| format!("encoding the materialized views: {e:?}"))?;
    // The archive is read back without validation — `DynValue` does not derive
    // the checks rkyv would need — so a checksum stands in for them. The storage
    // directory is trusted; what this guards against is a torn or bit-rotted
    // file being read as rows.
    let sum = xxhash_rust::xxh3::xxh3_64(&bytes);
    let path = dir.join(FILE);
    let mut file =
        std::fs::File::create(&path).map_err(|e| format!("writing `{}`: {e}", path.display()))?;
    file.write_all(&sum.to_le_bytes())
        .and_then(|()| file.write_all(&bytes))
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("writing `{}`: {e}", path.display()))
}

/// Reads back the rows of every view in `wanted` saved with checkpoint `uuid`.
///
/// Call only after `Runner::build` has accepted this checkpoint's manifest: that
/// check is what establishes these bytes were written in this build's value
/// format.
///
/// A view that is materialized now and was not saved then is refused rather
/// than started empty, since starting it empty is the wrong answer this module
/// exists to prevent.
pub fn load(root: &Path, uuid: &str, wanted: &[String]) -> Result<HashMap<String, Rows>, String> {
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let path = root.join(uuid).join(FILE);
    let file = std::fs::read(&path).map_err(|e| {
        format!(
            "reading `{}`: {e}. The configuration materializes {}, and a restored view's \
             snapshot has to start from the rows the checkpoint held — without this file it \
             would start empty and report the view as having none.",
            path.display(),
            names(wanted.iter())
        )
    })?;
    let corrupt = || {
        format!(
            "`{}` does not match the checksum it was written with, so its rows cannot be \
             trusted. Resume from another checkpoint.",
            path.display()
        )
    };
    if file.len() < 8 {
        return Err(corrupt());
    }
    let (sum, bytes) = file.split_at(8);
    let sum = u64::from_le_bytes(sum.try_into().map_err(|_| corrupt())?);
    if xxhash_rust::xxh3::xxh3_64(bytes) != sum {
        return Err(corrupt());
    }

    let mut aligned = rkyv::AlignedVec::with_capacity(bytes.len());
    aligned.extend_from_slice(bytes);
    // SAFETY: the checksum says these are the bytes `save` wrote, and the
    // manifest check that precedes every call says they were written in this
    // build's value format — the only other way they could mean something else.
    let archived = unsafe { rkyv::archived_root::<Saved>(&aligned) };
    let saved: Saved = archived
        .deserialize(&mut rkyv::Infallible)
        .map_err(|e| format!("reading `{}`: {e:?}", path.display()))?;

    let mut by_name: HashMap<String, Rows> = saved
        .into_iter()
        .map(|(name, rows)| {
            let rows = rows
                .into_iter()
                .map(|(key, value, weight)| ((key, value), weight))
                .collect();
            (name, rows)
        })
        .collect();

    let missing: Vec<&String> = wanted
        .iter()
        .filter(|w| !by_name.contains_key(*w))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the configuration materializes {} but checkpoint {uuid} was taken without {}, \
             so {} snapshot would start empty and report rows the view holds as absent. \
             Remove {} from `materialized`, or take a new checkpoint with {} materialized.",
            names(missing.iter().copied()),
            names(missing.iter().copied()),
            if missing.len() == 1 { "its" } else { "their" },
            if missing.len() == 1 { "it" } else { "them" },
            if missing.len() == 1 { "it" } else { "them" },
        ));
    }
    Ok(wanted
        .iter()
        .filter_map(|w| by_name.remove(w).map(|rows| (w.clone(), rows)))
        .collect())
}

fn names<'a>(names: impl Iterator<Item = &'a String>) -> String {
    names
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
