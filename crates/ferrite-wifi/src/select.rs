//! Which of the configured networks to try for the BSSes a scan found, and how.
//!
//! Every configured network that has a BSS it can join is a candidate, and so is each such BSS; the
//! strongest signal goes first and a tie keeps the order of the file. The service tries them in that
//! order and only backs off once the last has failed.

use alloc::vec::Vec;

use ferrite_nl80211::nl80211::{Bss, Security};
use ferrite_wpa::env::{Allow, Auth, Network};
use ferrite_wpa::rsn::{Rsn, rsnx_has_h2e};
use ferrite_wpa::sae;

/// The most candidates one attempt tries; each can take a connect limit to fail.
pub const MAX_TRIES: usize = 4;
/// The most hidden names a scan probes for, which every driver's scan request takes with the wildcard.
pub const MAX_HIDDEN: usize = 4;

/// How a candidate is joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    Open,
    /// WPA2-PSK.
    Psk,
    /// WPA3-SAE, with the password element found the way the access point supports.
    Sae(sae::Method),
}

/// A network and a BSS of it that the station can join.
#[derive(Clone, Copy, Debug)]
pub struct Candidate<'a> {
    pub network: &'a Network,
    pub bss: &'a Bss,
    pub join: Join,
}

/// How `network` joins `bss`, if it can: the same name, and a security the network allows and the BSS
/// offers. SAE is chosen over PSK when both qualify.
pub fn join_for(network: &Network, bss: &Bss) -> Option<Join> {
    if bss.ssid != network.ssid.as_bytes() {
        return None;
    }
    match (&network.auth, bss.security) {
        (Auth::Open, Security::Open) => Some(Join::Open),
        (Auth::Personal { allow, .. }, Security::Rsn) => {
            let rsn = Rsn::parse(bss.rsn.as_deref()?.get(2..)?).ok()?;
            let sae = *allow != Allow::PskOnly && rsn.supports_sae_ccmp();
            if sae {
                let h2e = bss.h2e_only || bss.rsnx.as_deref().is_some_and(rsnx_has_h2e);
                Some(Join::Sae(if h2e { sae::Method::HashToElement } else { sae::Method::HuntAndPeck }))
            } else if *allow != Allow::SaeOnly && rsn.supports_psk_ccmp() {
                Some(Join::Psk)
            } else {
                None
            }
        },
        _ => None,
    }
}

/// The names of the hidden networks, for a scan to probe for.
pub fn hidden_ssids(networks: &[Network]) -> Vec<&[u8]> {
    networks.iter().filter(|n| n.hidden).map(|n| n.ssid.as_bytes()).take(MAX_HIDDEN).collect()
}

/// The candidates for `bsses`, strongest first and at most [`MAX_TRIES`].
pub fn candidates<'a>(networks: &'a [Network], bsses: &'a [Bss]) -> Vec<Candidate<'a>> {
    let mut found: Vec<Candidate<'a>> = networks
        .iter()
        .flat_map(|network| {
            bsses.iter().filter_map(move |bss| join_for(network, bss).map(|join| Candidate { network, bss, join }))
        })
        .collect();
    found.sort_by_key(|c| core::cmp::Reverse(c.bss.signal.map_or(i32::MIN, |s| s.0)));
    found.truncate(MAX_TRIES);
    found
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use assert2::{assert, check};
    use ferrite_nl80211::nl80211::{Frequency, Mbm, SsidDisplay};
    use ferrite_wpa::kdf::{Passphrase, Ssid};
    use ferrite_wpa::rsn::{CAP_MFPC, CAP_MFPR, RSNX_H2E_ELEMENT, Suite};
    use rstest::{fixture, rstest};

    use super::*;

    fn network(ssid: &str, auth: Auth) -> Network {
        Network { ssid: Ssid::new(ssid.as_bytes()).unwrap(), hidden: false, auth }
    }

    fn personal(ssid: &str, allow: Allow) -> Network {
        network(ssid, Auth::Personal { passphrase: Passphrase::new(b"passphrase").unwrap(), allow })
    }

    fn bss(ssid: &str, signal: i32, security: Security, rsn: Option<&Rsn>) -> Bss {
        Bss {
            bssid: ferrite_nl80211::nl80211::MacAddr([2, 0, 0, 0, 0, signal.unsigned_abs() as u8]),
            frequency: Frequency(2412),
            signal: Some(Mbm(signal)),
            ssid: ssid.as_bytes().to_vec(),
            security,
            rsn: rsn.map(Rsn::element),
            rsnx: None,
            h2e_only: false,
        }
    }

    fn rsn(akms: &[Suite], capabilities: u16) -> Rsn {
        let group_mgmt = (capabilities & CAP_MFPC != 0).then_some(Suite::BIP_CMAC_128);
        Rsn { akms: akms.to_vec(), capabilities, group_mgmt, ..Rsn::psk_ccmp() }
    }

    #[fixture]
    fn wpa2() -> Rsn {
        rsn(&[Suite::PSK], 0)
    }

    #[fixture]
    fn wpa3() -> Rsn {
        rsn(&[Suite::SAE], CAP_MFPR | CAP_MFPC)
    }

    #[fixture]
    fn transition() -> Rsn {
        rsn(&[Suite::PSK, Suite::SAE], CAP_MFPC)
    }

    #[rstest]
    #[case::open_network_open_bss(Auth::Open, Security::Open, None, Some(Join::Open))]
    #[case::open_network_secured_bss(Auth::Open, Security::Rsn, Some(wpa2()), None)]
    #[case::passphrase_network_open_bss(
        Auth::Personal { passphrase: Passphrase::new(b"passphrase").unwrap(), allow: Allow::Any },
        Security::Open,
        None,
        None
    )]
    fn open_and_secured_do_not_mix(
        #[case] auth: Auth,
        #[case] security: Security,
        #[case] rsn: Option<Rsn>,
        #[case] want: Option<Join>,
    ) {
        check!(join_for(&network("net", auth), &bss("net", -40, security, rsn.as_ref())) == want);
    }

    #[rstest]
    #[case::any_wpa2(Allow::Any, wpa2(), Some(Join::Psk))]
    #[case::any_wpa3(Allow::Any, wpa3(), Some(Join::Sae(sae::Method::HuntAndPeck)))]
    #[case::any_prefers_sae_in_transition(Allow::Any, transition(), Some(Join::Sae(sae::Method::HuntAndPeck)))]
    #[case::psk_only_takes_psk_in_transition(Allow::PskOnly, transition(), Some(Join::Psk))]
    #[case::psk_only_refuses_wpa3(Allow::PskOnly, wpa3(), None)]
    #[case::sae_only_refuses_wpa2(Allow::SaeOnly, wpa2(), None)]
    #[case::sae_only_takes_sae_in_transition(Allow::SaeOnly, transition(), Some(Join::Sae(sae::Method::HuntAndPeck)))]
    fn the_allowed_key_management_and_the_offered_ones_pick_the_join(
        #[case] allow: Allow,
        #[case] offered: Rsn,
        #[case] want: Option<Join>,
    ) {
        check!(join_for(&personal("net", allow), &bss("net", -40, Security::Rsn, Some(&offered))) == want);
    }

    #[rstest]
    #[case::none(false, false, sae::Method::HuntAndPeck)]
    #[case::rsnx(true, false, sae::Method::HashToElement)]
    #[case::selector(false, true, sae::Method::HashToElement)]
    fn an_access_point_that_does_hash_to_element_gets_it(
        wpa3: Rsn,
        #[case] has_rsnx: bool,
        #[case] h2e_only: bool,
        #[case] method: sae::Method,
    ) {
        let mut found = bss("net", -40, Security::Rsn, Some(&wpa3));
        found.rsnx = has_rsnx.then(|| RSNX_H2E_ELEMENT.to_vec());
        found.h2e_only = h2e_only;
        check!(join_for(&personal("net", Allow::Any), &found) == Some(Join::Sae(method)));
    }

    #[rstest]
    fn another_name_never_matches(wpa2: Rsn) {
        check!(join_for(&personal("net", Allow::Any), &bss("other", -40, Security::Rsn, Some(&wpa2))).is_none());
    }

    fn names(found: &[Candidate<'_>]) -> Vec<(String, i32)> {
        found
            .iter()
            .map(|c| (alloc::format!("{}", SsidDisplay(c.network.ssid.as_bytes())), c.bss.signal.unwrap().0))
            .collect()
    }

    #[rstest]
    fn the_strongest_network_goes_first_and_a_tie_keeps_the_file_order(wpa2: Rsn) {
        let networks = [personal("a", Allow::Any), personal("b", Allow::Any), personal("c", Allow::Any)];
        let bsses = [
            bss("c", -50, Security::Rsn, Some(&wpa2)),
            bss("a", -70, Security::Rsn, Some(&wpa2)),
            bss("b", -50, Security::Rsn, Some(&wpa2)),
            bss("elsewhere", -10, Security::Rsn, Some(&wpa2)),
        ];
        let found = candidates(&networks, &bsses);
        check!(names(&found) == [("b".into(), -50), ("c".into(), -50), ("a".into(), -70)]);
    }

    #[rstest]
    fn every_bss_of_a_network_is_a_candidate_but_no_more_than_the_limit(wpa2: Rsn) {
        let networks = [personal("a", Allow::Any)];
        let bsses: Vec<Bss> = (1..=6).map(|n| bss("a", -10 * n, Security::Rsn, Some(&wpa2))).collect();
        let found = candidates(&networks, &bsses);
        assert!(found.len() == MAX_TRIES);
        check!(names(&found).iter().map(|(_, signal)| *signal).collect::<Vec<_>>() == [-10, -20, -30, -40]);
    }

    #[test]
    fn a_scan_probes_for_the_hidden_names_only_and_not_for_more_than_the_limit() {
        let networks: Vec<Network> =
            (0..6).map(|n| Network { hidden: n != 1, ..personal(&alloc::format!("net{n}"), Allow::Any) }).collect();
        let names = hidden_ssids(&networks);
        check!(names == [&b"net0"[..], b"net2", b"net3", b"net4"]);
        check!(hidden_ssids(&[personal("a", Allow::Any)]).is_empty());
    }

    #[test]
    fn nothing_visible_means_no_candidates() {
        check!(candidates(&[personal("a", Allow::Any)], &[]).is_empty());
    }
}
