// SPDX-License-Identifier: GPL-2.0
/*
 * Replaces drivers/base/firmware_loader/main.c. Every request is one call to
 * the shim's firmware provider, which returns the file's bytes by name; there
 * is no user-space helper, no cache and no fallback. The asynchronous request
 * runs its callback from a work item on system_wq, as Linux does.
 */

#include <linux/device.h>
#include <linux/export.h>
#include <linux/firmware.h>
#include <linux/printk.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/workqueue.h>

#include <lx/firmware.h>

static int lx_request_firmware(const struct firmware **firmware_p, const char *name, struct device *device,
			       bool warn)
{
	struct firmware *fw;
	u8 *data;
	size_t size;
	int ret;

	if (!firmware_p)
		return -EINVAL;
	*firmware_p = NULL;
	if (!name || name[0] == '\0')
		return -EINVAL;

	might_sleep();
	fw = kzalloc(sizeof(*fw), GFP_KERNEL);
	if (!fw)
		return -ENOMEM;

	ret = lx_firmware_load(name, &data, &size);
	if (ret) {
		if (warn)
			dev_warn(device, "Direct firmware load for %s failed with error %d\n", name, ret);
		kfree(fw);
		return ret;
	}

	fw->data = data;
	fw->size = size;
	*firmware_p = fw;
	return 0;
}

int request_firmware(const struct firmware **firmware_p, const char *name, struct device *device)
{
	return lx_request_firmware(firmware_p, name, device, true);
}
EXPORT_SYMBOL(request_firmware);

int firmware_request_nowarn(const struct firmware **firmware, const char *name, struct device *device)
{
	return lx_request_firmware(firmware, name, device, false);
}
EXPORT_SYMBOL_GPL(firmware_request_nowarn);

int request_firmware_direct(const struct firmware **firmware_p, const char *name, struct device *device)
{
	return lx_request_firmware(firmware_p, name, device, true);
}
EXPORT_SYMBOL_GPL(request_firmware_direct);

void release_firmware(const struct firmware *fw)
{
	if (!fw)
		return;
	lx_firmware_free((u8 *)fw->data, fw->size);
	kfree(fw);
}
EXPORT_SYMBOL(release_firmware);

struct firmware_work {
	struct work_struct work;
	const char *name;
	struct device *device;
	void *context;
	void (*cont)(const struct firmware *fw, void *context);
};

static void request_firmware_work_func(struct work_struct *work)
{
	struct firmware_work *fw_work = container_of(work, struct firmware_work, work);
	const struct firmware *fw;

	lx_request_firmware(&fw, fw_work->name, fw_work->device, true);
	fw_work->cont(fw, fw_work->context);
	put_device(fw_work->device);
	kfree(fw_work->name);
	kfree(fw_work);
}

int request_firmware_nowait(struct module *module, bool uevent, const char *name, struct device *device,
			    gfp_t gfp, void *context,
			    void (*cont)(const struct firmware *fw, void *context))
{
	struct firmware_work *fw_work;

	fw_work = kzalloc(sizeof(*fw_work), gfp);
	if (!fw_work)
		return -ENOMEM;

	fw_work->name = kstrdup(name, gfp);
	if (!fw_work->name) {
		kfree(fw_work);
		return -ENOMEM;
	}
	fw_work->device = get_device(device);
	fw_work->context = context;
	fw_work->cont = cont;
	INIT_WORK(&fw_work->work, request_firmware_work_func);
	schedule_work(&fw_work->work);
	return 0;
}
EXPORT_SYMBOL(request_firmware_nowait);
