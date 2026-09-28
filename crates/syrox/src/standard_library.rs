use syrox_engine::{AuthenticatedStandardLibrary, CheckConfiguration};

pub(crate) fn configuration(enabled: bool) -> CheckConfiguration {
    let mut configuration = CheckConfiguration::default();
    if enabled {
        configuration.standard_library = Some(AuthenticatedStandardLibrary::bundled());
    }
    configuration
}
