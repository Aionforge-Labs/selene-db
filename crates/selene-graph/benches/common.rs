#![allow(missing_docs)]
#![allow(dead_code)]

use std::time::Duration;

use criterion::Criterion;
use selene_core::Change;
use selene_graph::{IndexProvider, ProviderError, ProviderTag};
use selene_testing::{BenchFixture, BenchProfile};

pub(crate) fn criterion_config() -> Criterion {
    let profile = BenchProfile::from_env();
    Criterion::default()
        .sample_size(profile.sample_size())
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_millis(match profile {
            BenchProfile::Quick => 500,
            BenchProfile::Full | BenchProfile::Stress => 1_500,
            _ => 500,
        }))
}

pub(crate) fn fixtures() -> Vec<BenchFixture> {
    BenchProfile::from_env()
        .scales()
        .iter()
        .copied()
        .map(BenchFixture::build)
        .collect()
}

#[derive(Clone, Copy)]
enum ProviderBehavior {
    Noop,
    Error,
    Panic,
}

struct BenchProvider {
    tag: ProviderTag,
    behavior: ProviderBehavior,
}

impl BenchProvider {
    const fn new(tag: ProviderTag, behavior: ProviderBehavior) -> Self {
        Self { tag, behavior }
    }
}

impl IndexProvider for BenchProvider {
    fn provider_tag(&self) -> ProviderTag {
        self.tag
    }

    fn on_change(&self, _change: &Change) -> Result<(), ProviderError> {
        match self.behavior {
            ProviderBehavior::Noop => Ok(()),
            ProviderBehavior::Error => Err(ProviderError::Inconsistent {
                reason: "synthetic provider fanout failure".to_owned(),
            }),
            ProviderBehavior::Panic => panic!("synthetic provider fanout panic"),
        }
    }
}

pub(crate) fn noop_provider(tag: ProviderTag) -> impl IndexProvider {
    BenchProvider::new(tag, ProviderBehavior::Noop)
}

pub(crate) fn error_provider(tag: ProviderTag) -> impl IndexProvider {
    BenchProvider::new(tag, ProviderBehavior::Error)
}

pub(crate) fn panicking_provider(tag: ProviderTag) -> impl IndexProvider {
    BenchProvider::new(tag, ProviderBehavior::Panic)
}

pub(crate) fn provider_tag(idx: usize) -> ProviderTag {
    let mut bytes = *b"P000";
    bytes[1] = b'0' + ((idx / 100) % 10) as u8;
    bytes[2] = b'0' + ((idx / 10) % 10) as u8;
    bytes[3] = b'0' + (idx % 10) as u8;
    ProviderTag(bytes)
}
