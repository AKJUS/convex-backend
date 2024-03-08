#[derive(Clone, Debug, PartialEq, strum::EnumString, strum::Display)]
#[strum(serialize_all = "snake_case")]
#[cfg_attr(any(test, feature = "testing"), derive(proptest_derive::Arbitrary))]
pub enum BackendState {
    Disabled,
    Paused,
    Running,
}
