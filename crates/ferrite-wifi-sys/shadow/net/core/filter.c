// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces net/core/filter.c, the BPF socket filters. Nothing can attach one (there is no setsockopt
 * from userspace and no BPF), so receiving runs only the checks stock makes before the filter.
 */

#include <linux/filter.h>
#include <linux/skbuff.h>
#include <net/ip.h>
#include <net/sock.h>

int sk_filter_trim_cap(struct sock *sk, struct sk_buff *skb, unsigned int cap, enum skb_drop_reason *reason)
{
	if (skb_pfmemalloc(skb) && !sock_flag(sk, SOCK_MEMALLOC)) {
		NET_INC_STATS(sock_net(sk), LINUX_MIB_PFMEMALLOCDROP);
		*reason = SKB_DROP_REASON_PFMEMALLOC;
		return -ENOMEM;
	}
	WARN_ON_ONCE(rcu_access_pointer(sk->sk_filter));
	return 0;
}
EXPORT_SYMBOL(sk_filter_trim_cap);
