// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces net/core/flow_dissector.c (BPF dissectors, tunnels, every L3 and L4 protocol) with the one
 * entry mac80211 reaches: the software flow hash queue selection sets on every transmitted frame.
 * IPv4 and IPv6 flows hash their addresses, protocol and TCP or UDP ports under a boot-random seed,
 * as stock does; any other protocol (EAPOL, ARP) hashes its EtherType alone. The hash orders flows
 * for fairness only, so the exact values stock computes are not needed.
 */

#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/ipv6.h>
#include <linux/jhash.h>
#include <linux/random.h>
#include <linux/skbuff.h>
#include <linux/tcp.h>
#include <linux/udp.h>
#include <net/ip.h>

struct lx_flow {
	u32 addrs[8];
	__be16 proto;
	u8 ip_proto;
	u8 pad;
	__be32 ports;
};

static u32 lx_flow_seed;

static bool lx_flow_ports(const struct sk_buff *skb, unsigned int off, u8 ip_proto, struct lx_flow *flow)
{
	__be32 ports;

	if (ip_proto != IPPROTO_TCP && ip_proto != IPPROTO_UDP)
		return false;
	if (skb_copy_bits(skb, off, &ports, sizeof(ports)))
		return false;
	flow->ports = ports;
	return true;
}

void __skb_get_hash_net(const struct net *net, struct sk_buff *skb)
{
	int off = skb_network_offset(skb);
	struct lx_flow flow = {};
	bool l4 = false;
	u32 hash;

	if (!lx_flow_seed)
		lx_flow_seed = get_random_u32() | 1;

	flow.proto = skb->protocol;
	if (!flow.proto && skb_headlen(skb) >= ETH_HLEN && skb_mac_header_was_set(skb))
		flow.proto = eth_hdr(skb)->h_proto;
	if (off <= 0 || off >= skb->len)
		off = ETH_HLEN;

	if (flow.proto == htons(ETH_P_IP)) {
		struct iphdr iph;

		if (!skb_copy_bits(skb, off, &iph, sizeof(iph)) && iph.ihl >= 5) {
			flow.addrs[0] = (__force u32)iph.saddr;
			flow.addrs[1] = (__force u32)iph.daddr;
			flow.ip_proto = iph.protocol;
			if (!(iph.frag_off & htons(IP_MF | IP_OFFSET)))
				l4 = lx_flow_ports(skb, off + iph.ihl * 4, iph.protocol, &flow);
		}
	} else if (flow.proto == htons(ETH_P_IPV6)) {
		struct ipv6hdr ip6h;

		if (!skb_copy_bits(skb, off, &ip6h, sizeof(ip6h))) {
			memcpy(&flow.addrs[0], &ip6h.saddr, 16);
			memcpy(&flow.addrs[4], &ip6h.daddr, 16);
			flow.ip_proto = ip6h.nexthdr;
			l4 = lx_flow_ports(skb, off + sizeof(ip6h), ip6h.nexthdr, &flow);
		}
	}

	hash = jhash2((const u32 *)&flow, sizeof(flow) / sizeof(u32), lx_flow_seed);
	__skb_set_sw_hash(skb, hash ?: 1, l4);
}
EXPORT_SYMBOL(__skb_get_hash_net);
