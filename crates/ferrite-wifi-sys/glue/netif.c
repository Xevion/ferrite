// SPDX-License-Identifier: GPL-2.0
/*
 * Ethernet frames between a net_device and the program, as bridge and macvlan take them: an rx_handler
 * copies each received frame out and consumes the skb, and a kthread sends the program's frames with
 * dev_queue_xmit. The program supplies the callbacks; it never runs Linux code outside a task.
 */

#include <linux/etherdevice.h>
#include <linux/if_ether.h>
#include <linux/kthread.h>
#include <linux/netdevice.h>
#include <linux/rtnetlink.h>
#include <linux/sched.h>
#include <linux/skbuff.h>
#include <linux/slab.h>
#include <net/net_namespace.h>

#define LX_NETIF_FRAME_MAX (ETH_HLEN + ETH_DATA_LEN)
#define LX_NETIF_GROUPS 4

struct lx_netif {
	struct net_device *dev;
	netdevice_tracker tracker;
	struct task_struct *tx_task;
	const void *ctx;
	/* A NULL frame is one the handler could not copy out; the program counts it dropped. */
	void (*rx)(const void *ctx, const u8 *frame, unsigned int len);
	/* The next frame to send copied into buf: its length, 0 when none is queued, < 0 to skip one. */
	int (*pull)(const void *ctx, u8 *buf, unsigned int cap);
	void (*sent)(const void *ctx, int rc);
	/* Multicast addresses joined for the program, left again on detach. */
	u8 groups[LX_NETIF_GROUPS][ETH_ALEN];
	unsigned int n_groups;
	u8 rx_buf[LX_NETIF_FRAME_MAX];
	u8 tx_buf[LX_NETIF_FRAME_MAX];
};

int lx_netif_lookup(int ifindex, u8 *mac, unsigned int *mtu)
{
	struct net_device *dev;
	int err = -ENODEV;

	rtnl_lock();
	dev = __dev_get_by_index(&init_net, ifindex);
	if (dev) {
		memcpy(mac, dev->dev_addr, ETH_ALEN);
		*mtu = dev->mtu;
		err = 0;
	}
	rtnl_unlock();
	return err;
}

static rx_handler_result_t lx_netif_rx_handler(struct sk_buff **pskb)
{
	struct sk_buff *skb = *pskb;
	struct lx_netif *netif = rcu_dereference(skb->dev->rx_handler_data);
	int mac_len;
	unsigned int len;

	/* mac80211 hands EAPOL to the nl80211 control port before here; any that arrives goes on as usual. */
	if (skb->protocol == htons(ETH_P_PAE))
		return RX_HANDLER_PASS;
	mac_len = skb_mac_header_was_set(skb) ? skb->data - skb_mac_header(skb) : -1;
	len = skb->len + mac_len;
	if (mac_len == ETH_HLEN && len <= sizeof(netif->rx_buf) &&
	    !skb_copy_bits(skb, -mac_len, netif->rx_buf, len))
		netif->rx(netif->ctx, netif->rx_buf, len);
	else
		netif->rx(netif->ctx, NULL, len);
	consume_skb(skb);
	return RX_HANDLER_CONSUMED;
}

static int lx_netif_xmit(struct lx_netif *netif, unsigned int len)
{
	struct net_device *dev = netif->dev;
	struct sk_buff *skb;
	int headroom = LL_RESERVED_SPACE(dev);

	if (len < ETH_HLEN)
		return -EINVAL;
	skb = alloc_skb(headroom + len + dev->needed_tailroom, GFP_KERNEL);
	if (!skb)
		return -ENOMEM;
	skb_reserve(skb, headroom);
	skb_put_data(skb, netif->tx_buf, len);
	skb_reset_mac_header(skb);
	skb_set_network_header(skb, ETH_HLEN);
	skb->protocol = eth_hdr(skb)->h_proto;
	skb->dev = dev;
	return dev_queue_xmit(skb);
}

/* Sleeps until woken with nothing queued; the check and the sleep cannot be split on one cooperative CPU. */
static int lx_netif_tx_thread(void *arg)
{
	struct lx_netif *netif = arg;
	int len;

	for (;;) {
		set_current_state(TASK_INTERRUPTIBLE);
		if (kthread_should_stop())
			break;
		len = netif->pull(netif->ctx, netif->tx_buf, sizeof(netif->tx_buf));
		if (len == 0) {
			schedule();
			continue;
		}
		__set_current_state(TASK_RUNNING);
		if (len > 0)
			netif->sent(netif->ctx, lx_netif_xmit(netif, len));
	}
	__set_current_state(TASK_RUNNING);
	return 0;
}

int lx_netif_attach(int ifindex, const void *ctx,
		    void (*rx)(const void *, const u8 *, unsigned int),
		    int (*pull)(const void *, u8 *, unsigned int),
		    void (*sent)(const void *, int), struct lx_netif **res)
{
	struct lx_netif *netif;
	int err;

	netif = kzalloc(sizeof(*netif), GFP_KERNEL);
	if (!netif)
		return -ENOMEM;
	netif->ctx = ctx;
	netif->rx = rx;
	netif->pull = pull;
	netif->sent = sent;
	netif->tx_task = kthread_run(lx_netif_tx_thread, netif, "lx-netif-tx/%d", ifindex);
	if (IS_ERR(netif->tx_task)) {
		err = PTR_ERR(netif->tx_task);
		kfree(netif);
		return err;
	}
	rtnl_lock();
	netif->dev = netdev_get_by_index(&init_net, ifindex, &netif->tracker, GFP_KERNEL);
	err = netif->dev ? netdev_rx_handler_register(netif->dev, lx_netif_rx_handler, netif) : -ENODEV;
	if (err && netif->dev)
		netdev_put(netif->dev, &netif->tracker);
	rtnl_unlock();
	if (err) {
		kthread_stop(netif->tx_task);
		kfree(netif);
		return err;
	}
	*res = netif;
	return 0;
}

/*
 * Receive frames to the Ethernet group `mac` too: it joins the device's multicast list, which mac80211
 * turns into the card's filter. Without it the card drops multicast such as mDNS.
 */
int lx_netif_join(struct lx_netif *netif, const u8 *mac)
{
	int err;

	if (netif->n_groups >= LX_NETIF_GROUPS)
		return -ENOSPC;
	err = dev_mc_add(netif->dev, mac);
	if (!err)
		memcpy(netif->groups[netif->n_groups++], mac, ETH_ALEN);
	return err;
}

/* The transmit thread, which the program marks runnable itself: no Linux code may run on its stack. */
struct task_struct *lx_netif_tx_task(struct lx_netif *netif)
{
	return netif->tx_task;
}

void lx_netif_detach(struct lx_netif *netif)
{
	unsigned int i;

	kthread_stop(netif->tx_task);
	for (i = 0; i < netif->n_groups; i++)
		dev_mc_del(netif->dev, netif->groups[i]);
	rtnl_lock();
	netdev_rx_handler_unregister(netif->dev);
	netdev_put(netif->dev, &netif->tracker);
	rtnl_unlock();
	kfree(netif);
}
