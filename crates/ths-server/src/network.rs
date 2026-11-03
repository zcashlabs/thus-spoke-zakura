use std::sync::OnceLock;

use zcash_protocol::{consensus::BlockHeight, local_consensus::LocalNetwork};

/// NU7 activates at this height unless an instance was created with another one.
pub(crate) const DEFAULT_NU7_ACTIVATION_HEIGHT: u32 = 1;

/// The NU7 activation height of the instance this process serves. `init` and
/// `serve` set it from the height stored with the instance before they open the
/// wallet, so every network parameter in the process matches the node's schedule.
static NU7_ACTIVATION_HEIGHT: OnceLock<u32> = OnceLock::new();

/// Selects this process's NU7 activation height. One process serves one
/// instance, so a second, different height is refused rather than applied.
pub(crate) fn use_nu7_activation_height(height: u32) -> anyhow::Result<()> {
    let active = *NU7_ACTIVATION_HEIGHT.get_or_init(|| height);
    anyhow::ensure!(
        active == height,
        "this process already uses NU7 activation height {active}, not {height}"
    );
    Ok(())
}

/// The default applies until `init` or `serve` selects a height, which unit
/// tests never do.
pub(crate) fn nu7_activation_height() -> u32 {
    NU7_ACTIVATION_HEIGHT
        .get()
        .copied()
        .unwrap_or(DEFAULT_NU7_ACTIVATION_HEIGHT)
}

pub(crate) fn regtest_network() -> LocalNetwork {
    regtest_network_with_nu7(nu7_activation_height())
}

/// Every upgrade through NU6.3 activates at height 1, so the Orchard pool never
/// accepts deposits on this chain and all new shielded value lives in Ironwood,
/// whichever height NU7 uses. Keep these heights in sync with the Zakura
/// configuration in `bootstrap/config.rs`.
pub(crate) fn regtest_network_with_nu7(nu7_activation_height: u32) -> LocalNetwork {
    let one = Some(BlockHeight::from_u32(1));
    LocalNetwork {
        overwinter: one,
        sapling: one,
        blossom: one,
        heartwood: one,
        canopy: one,
        nu5: one,
        nu6: one,
        nu6_1: one,
        nu6_2: one,
        nu6_3: one,
        nu7: Some(BlockHeight::from_u32(nu7_activation_height)),
    }
}
