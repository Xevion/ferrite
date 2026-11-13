// SPDX-License-Identifier: GPL-2.0-only
/*
 * Replaces net/wireless/nl80211.c, the userspace interface, which is cut: no
 * generic netlink family is registered, and the shim drives cfg80211_ops
 * directly. Its calls from cfg80211 become no-ops or callbacks into the shim.
 */

#include <linux/init.h>

#include "nl80211.h"

int __init nl80211_init(void)
{
	return 0;
}

void nl80211_exit(void)
{
}
