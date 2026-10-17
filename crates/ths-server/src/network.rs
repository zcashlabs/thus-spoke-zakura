use zcash_protocol::{consensus::BlockHeight, local_consensus::LocalNetwork};

/// Every upgrade through NU7 activates at height 1, so the Orchard pool never
/// accepts deposits on this chain and all new shielded value lives in Ironwood.
/// Keep these heights in sync with the Zakura configuration in `bootstrap/config.rs`.
pub(crate) fn regtest_network() -> LocalNetwork {
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
        nu7: one,
    }
}
