// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces lib/kobject_uevent.c. Uevents go to userspace over netlink and a
 * helper binary, and nothing here listens, so an event is dropped and
 * reported as sent. add_uevent_var is only reached while building one, so it
 * stays a stub that names itself.
 */

#include <linux/kobject.h>

int kobject_uevent_env(struct kobject *kobj, enum kobject_action action, char *envp_ext[])
{
	return 0;
}

int kobject_uevent(struct kobject *kobj, enum kobject_action action)
{
	return 0;
}

int kobject_synth_uevent(struct kobject *kobj, const char *buf, size_t count)
{
	return 0;
}
