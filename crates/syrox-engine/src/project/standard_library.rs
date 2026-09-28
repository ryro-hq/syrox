use super::{AuthenticatedStandardLibrary, AuthenticatedStandardSource};

impl AuthenticatedStandardLibrary {
    /// The checked-in standard library embedded in the executable, with each
    /// source authenticated and inventoried separately in the project Lock.
    pub fn bundled() -> Self {
        let sources = [
            ("std/main.srx", include_str!("../../../../std/main.srx")),
            ("std/option.srx", include_str!("../../../../std/option.srx")),
            ("std/result.srx", include_str!("../../../../std/result.srx")),
            (
                "std/collections/list.srx",
                include_str!("../../../../std/collections/list.srx"),
            ),
            (
                "std/collections/map.srx",
                include_str!("../../../../std/collections/map.srx"),
            ),
            (
                "std/package.srx",
                include_str!("../../../../std/package.srx"),
            ),
            ("std/build.srx", include_str!("../../../../std/build.srx")),
        ]
        .into_iter()
        .map(|(name, text)| {
            AuthenticatedStandardSource::from_authenticated(name, text)
                .expect("checked-in standard-library source is valid")
        })
        .collect();
        Self::from_authenticated(sources).expect("checked-in standard-library metadata is valid")
    }
}
