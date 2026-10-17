use super::*;
use crate::args::SendAmount;

fn send_args(from: u8, to: u8, destination_pool: Pool, memo: Option<&str>) -> SendArgs {
    SendArgs {
        from,
        to: Some(to),
        uri: None,
        amount: Some(SendAmount(1)),
        max: false,
        source_pool: Pool::Ironwood,
        destination_pool,
        memo: memo.map(str::to_owned),
    }
}

#[test]
fn sends_allow_the_same_account_only_between_pools() {
    for to in [1, 2] {
        for source in [Pool::Ironwood, Pool::Transparent] {
            for destination in [Pool::Ironwood, Pool::Transparent] {
                let mut args = send_args(1, to, destination, None);
                args.source_pool = source;
                assert_eq!(check_send(&args).is_ok(), to != 1 || source != destination);
            }
        }
    }
    let mut shield = send_args(1, 1, Pool::Ironwood, Some("memo"));
    shield.source_pool = Pool::Transparent;
    assert!(check_send(&shield).is_ok());
    assert!(check_send(&send_args(1, 1, Pool::Transparent, Some("hi"))).is_err());
    assert!(check_send(&send_args(1, 1, Pool::Transparent, Some(""))).is_err());
}

#[test]
fn sends_are_checked_before_contacting_the_environment() {
    assert!(check_send(&send_args(1, 2, Pool::Ironwood, Some(""))).is_ok());
    assert!(check_send(&send_args(1, 2, Pool::Transparent, None)).is_ok());
    assert!(check_send(&send_args(3, 3, Pool::Ironwood, None)).is_err());
    assert!(check_send(&send_args(1, 2, Pool::Transparent, Some("hi"))).is_err());
    assert!(check_send(&send_args(1, 2, Pool::Transparent, Some(""))).is_err());
}
