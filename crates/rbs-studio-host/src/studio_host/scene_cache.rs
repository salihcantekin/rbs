//! Cached `scene.list` answer.
//!
//! [`answer`](self::answer) returns the list [`Host::publish`](super::Host::publish) already
//! built when the inputs below are unchanged. A miss builds the same value
//! [`super::api_scenes`] would build from a fresh [`super::Host::scene_list`].
//!
//! The cached value is a function of the collection epoch, the collection generation,
//! the studio canvas, the program scene and the stinger's found cut point:
//! - every edit of a [`rbs_scene::SceneCollection`] commits through `apply`, which
//!   bumps `generation`;
//! - every replacement of the whole collection bumps the epoch first;
//! - `scene.list` reads the program from `Host::scene`, the preview, master, glide,
//!   transition and sources from that collection and canvas, and the found cut from
//!   the stinger. The stinger length, the look-in-progress flag and the alpha warning
//!   are on the UI snapshot only; they are not in the control API answer.

use rbs_api::types;

use super::Host;

/// Inputs of the control API scene list. Equal keys mean a fresh build would return
/// the stored value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SceneAnswerKey {
    epoch: u64,
    generation: u64,
    canvas: u32,
    program: u32,
    cut_found_ms: Option<u32>,
}

impl SceneAnswerKey {
    pub(super) fn of(host: &Host) -> Self {
        Self {
            epoch: host.collection_epoch,
            generation: host.collection.generation(),
            canvas: host.canvas.get(),
            program: host.scene.get(),
            cut_found_ms: host.stinger.cut_found_ms(),
        }
    }
}

/// The last `scene.list` body stored for [`SceneAnswerKey`].
#[derive(Debug, Default)]
pub(super) struct SceneAnswerCache {
    key: Option<SceneAnswerKey>,
    list: Option<types::SceneList>,
}

impl SceneAnswerCache {
    /// Remembers `list` as the answer for `key`, replacing whatever was stored.
    pub(super) fn store(&mut self, key: SceneAnswerKey, list: types::SceneList) {
        self.key = Some(key);
        self.list = Some(list);
    }

    /// `list` already stored for `key`, or the value `build` returns (then stored).
    pub(super) fn get_or_build(
        &mut self,
        key: SceneAnswerKey,
        build: impl FnOnce() -> types::SceneList,
    ) -> types::SceneList {
        if self.key == Some(key)
            && let Some(list) = &self.list
        {
            return list.clone();
        }
        let list = build();
        self.store(key, list.clone());
        list
    }
}

/// The `scene.list` body. A second call with the same snapshot clones the stored
/// answer and does not walk the collection.
pub(super) fn answer(host: &mut Host) -> types::SceneList {
    let key = SceneAnswerKey::of(host);
    let mut cache = std::mem::take(&mut host.scene_answer);
    let list = cache.get_or_build(key, || super::api_scenes(&host.scene_list()));
    host.scene_answer = cache;
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(generation: u64, program: u32, cut_found_ms: Option<u32>) -> SceneAnswerKey {
        SceneAnswerKey {
            epoch: 1,
            generation,
            canvas: 1,
            program,
            cut_found_ms,
        }
    }

    fn listed(program: u32) -> types::SceneList {
        types::SceneList {
            scenes: Vec::new(),
            program,
            preview: None,
            master: types::Master::default(),
            glide: types::Glide::default(),
            transition: types::Transition::default(),
            sources: Vec::new(),
        }
    }

    #[test]
    fn a_second_poll_with_the_same_snapshot_does_not_rebuild() {
        let mut cache = SceneAnswerCache::default();
        let mut builds = 0;
        let first = cache.get_or_build(key(4, 7, None), || {
            builds += 1;
            listed(7)
        });
        let second = cache.get_or_build(key(4, 7, None), || {
            builds += 1;
            listed(99)
        });
        assert_eq!(builds, 1);
        assert_eq!(first, second);
        assert_eq!(second.program, 7);

        // What publish stored is the poll answer; the builder does not run.
        cache.store(key(4, 7, Some(12)), listed(7));
        let published = cache.get_or_build(key(4, 7, Some(12)), || {
            builds += 1;
            listed(99)
        });
        assert_eq!(builds, 1);
        assert_eq!(published.program, 7);

        // A new generation, program or found cut is not served from the old body.
        let changed = cache.get_or_build(key(5, 7, Some(12)), || {
            builds += 1;
            listed(8)
        });
        assert_eq!(builds, 2);
        assert_eq!(changed.program, 8);
        let cut = cache.get_or_build(key(5, 7, Some(40)), || {
            builds += 1;
            listed(8)
        });
        assert_eq!(builds, 3);
        assert_eq!(cut, listed(8));
    }
}
