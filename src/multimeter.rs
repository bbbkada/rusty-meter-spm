/// Meter measurement modes supported by the application.
///
/// Which modes a specific device actually supports is determined by the
/// device plugin's [`DeviceCapabilities::supported_modes`](crate::plugins::DeviceCapabilities).
#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub enum MeterMode {
    Vdc,
    Vac,
    Adc,
    Aac,
    Res,
    Cap,
    Freq,
    Per,
    Diod,
    Cont,
    Temp,
}
