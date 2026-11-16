//! The banners the user hid once they knew them (D-147): for each subject,
//! the causes hidden. A banner stays hidden while it tells only hidden
//! causes; a cause not hidden yet (a GPU that was at its power limit and
//! now runs hot) shows it again. Kept for the Viewer's run only.

use std::collections::BTreeMap;

use dgxtop_core::view::{Banner, BannerTopic};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HiddenBanners {
    causes_by_subject: BTreeMap<String, u64>,
}

impl HiddenBanners {
    /// Hides every banner in `shown` that may be hidden; returns whether a
    /// banner was shown that is now hidden.
    pub fn hide_all(&mut self, shown: &[Banner]) -> bool {
        let mut hid_one = false;
        for topic in shown.iter().filter_map(|banner| banner.topic.as_ref()) {
            if self.hides(topic) {
                continue;
            }
            *self
                .causes_by_subject
                .entry(topic.subject.clone())
                .or_default() |= topic.causes;
            hid_one = true;
        }
        hid_one
    }

    /// Whether the user hid every cause `topic` tells.
    pub fn hides(&self, topic: &BannerTopic) -> bool {
        let hidden = self
            .causes_by_subject
            .get(&topic.subject)
            .copied()
            .unwrap_or(0);
        topic.causes & !hidden == 0
    }

    /// Whether `banner` is shown.
    pub fn shows(&self, banner: &Banner) -> bool {
        banner.topic.as_ref().is_none_or(|topic| !self.hides(topic))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dgxtop_core::view::BannerLevel;

    fn slowdown(gpu: &str, causes: u64) -> Banner {
        Banner {
            level: BannerLevel::Warning,
            text: format!("{gpu} slowed down"),
            topic: Some(BannerTopic {
                subject: format!("gpu-slowdown:{gpu}"),
                causes,
            }),
        }
    }

    /// Hidden causes stay hidden, also fewer of them; a new cause or
    /// another GPU shows; a banner without a topic is never hidden.
    #[test]
    fn hidden_causes_stay_hidden_and_a_new_cause_shows() {
        let mut hidden = HiddenBanners::default();
        let lasting = Banner::new(BannerLevel::Error, "audit failed");
        assert!(
            !hidden.hide_all(std::slice::from_ref(&lasting)),
            "nothing to hide"
        );
        assert!(hidden.shows(&lasting));

        assert!(hidden.hide_all(&[slowdown("gpu0", 0x4 | 0x400), lasting.clone()]));
        assert!(!hidden.shows(&slowdown("gpu0", 0x4 | 0x400)));
        assert!(!hidden.shows(&slowdown("gpu0", 0x400)), "fewer causes");
        assert!(hidden.shows(&slowdown("gpu0", 0x4 | 0x20)), "heat is new");
        assert!(hidden.shows(&slowdown("gpu1", 0x4)), "another GPU");
        assert!(hidden.shows(&lasting));

        assert!(!hidden.hide_all(&[slowdown("gpu0", 0x4)]), "already hidden");
        assert!(hidden.hide_all(&[slowdown("gpu0", 0x20)]));
        assert!(!hidden.shows(&slowdown("gpu0", 0x4 | 0x20 | 0x400)));
    }
}
