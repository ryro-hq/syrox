use syrox_engine::{AuthenticatedStandardLibrary, AuthenticatedStandardSource, CheckConfiguration};

const SOURCE_NAME: &str = "std/pkg.srx";
const SOURCE: &str = include_str!("../../../std/pkg.srx");

pub(crate) fn configuration(enabled: bool) -> CheckConfiguration {
    let mut configuration = CheckConfiguration::default();
    if enabled {
        let source = AuthenticatedStandardSource::from_authenticated(SOURCE_NAME, SOURCE)
            .expect("checked-in standard-library source is valid");
        configuration.standard_library = Some(
            AuthenticatedStandardLibrary::from_authenticated(vec![source])
                .expect("checked-in standard-library metadata is valid"),
        );
    }
    configuration
}
