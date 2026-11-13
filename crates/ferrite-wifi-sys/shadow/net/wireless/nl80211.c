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

/* No netlink socket listens, so a regulatory change has no one to be broadcast to. */
void nl80211_common_reg_change_event(enum nl80211_commands cmd_id, struct regulatory_request *request)
{
}

/* Wiphy and interface lifecycle events, likewise with no listener. */
void nl80211_notify_wiphy(struct cfg80211_registered_device *rdev, enum nl80211_commands cmd)
{
}

void nl80211_notify_iface(struct cfg80211_registered_device *rdev, struct wireless_dev *wdev,
			  enum nl80211_commands cmd)
{
}

/* Stock: a wiphy's coalesce rules, which only nl80211 sets, freed when it unregisters. */
void cfg80211_free_coalesce(struct cfg80211_coalesce *coalesce)
{
	struct cfg80211_coalesce_rules *rule;
	int i, j;

	if (!coalesce)
		return;

	for (i = 0; i < coalesce->n_rules; i++) {
		rule = &coalesce->rules[i];
		for (j = 0; j < rule->n_patterns; j++)
			kfree(rule->patterns[j].mask);
		kfree(rule->patterns);
	}
	kfree(coalesce);
}
