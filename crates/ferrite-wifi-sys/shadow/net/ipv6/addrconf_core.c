// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Replaces net/ipv6/addrconf_core.c: as with IPv4, interfaces never get Linux IPv6 addresses, so the
 * address chain never fires; registration works as in Linux.
 */

#include <linux/notifier.h>
#include <net/addrconf.h>

static ATOMIC_NOTIFIER_HEAD(inet6addr_chain);

int register_inet6addr_notifier(struct notifier_block *nb)
{
	return atomic_notifier_chain_register(&inet6addr_chain, nb);
}
EXPORT_SYMBOL(register_inet6addr_notifier);

int unregister_inet6addr_notifier(struct notifier_block *nb)
{
	return atomic_notifier_chain_unregister(&inet6addr_chain, nb);
}
EXPORT_SYMBOL(unregister_inet6addr_notifier);
