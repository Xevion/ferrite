// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces net/ipv4/devinet.c: the IP stack is not Linux's, so interfaces never get Linux IPv4
 * addresses and this chain never fires. Drivers and mac80211 still register on it as in Linux.
 */

#include <linux/inetdevice.h>
#include <linux/notifier.h>

static BLOCKING_NOTIFIER_HEAD(inetaddr_chain);

int register_inetaddr_notifier(struct notifier_block *nb)
{
	return blocking_notifier_chain_register(&inetaddr_chain, nb);
}
EXPORT_SYMBOL(register_inetaddr_notifier);

int unregister_inetaddr_notifier(struct notifier_block *nb)
{
	return blocking_notifier_chain_unregister(&inetaddr_chain, nb);
}
EXPORT_SYMBOL(unregister_inetaddr_notifier);
