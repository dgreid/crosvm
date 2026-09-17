/*
 * 1- and 2-byte reads of crosvm's PciVirtualConfigMmio window.
 *
 * With a guest RAM size at or under 4GiB the window starts at 4GiB.
 * Before the zero-fill fix, PciVirtualConfigMmio::read copies 4 bytes
 * into this shorter buffer and aborts crosvm. After the fix the handler
 * zero-fills only the requested length and these reads return 0.
 *
 * Build against the guest kernel. See README.md.
 */

#include <linux/init.h>
#include <linux/io.h>
#include <linux/ioport.h>
#include <linux/kernel.h>
#include <linux/module.h>

MODULE_LICENSE("GPL");
MODULE_AUTHOR("Defensive VMM research (crosvm PciVirtualConfigMmio)");
MODULE_DESCRIPTION("Short reads of crosvm PciVirtualConfigMmio");
MODULE_VERSION("1.0");

static unsigned long pci_vcfg_base = 0x100000000UL;
module_param(pci_vcfg_base, ulong, 0644);
MODULE_PARM_DESC(pci_vcfg_base,
		 "GPA of PciVirtualConfigMmio (default 0x100000000)");

static int __init pci_vcfg_short_read_init(void)
{
	void __iomem *map;
	struct resource *held;
	u8 b;
	u16 w;
	const unsigned long map_len = 0x1000;

	pr_info("pci_vcfg_short_read: base=0x%lx\n", pci_vcfg_base);

	held = request_mem_region(pci_vcfg_base, map_len, "pci_vcfg_short_read");
	if (!held)
		pr_warn("pci_vcfg_short_read: request_mem_region failed; will not release\n");

	map = ioremap(pci_vcfg_base, map_len);
	if (!map) {
		pr_err("pci_vcfg_short_read: ioremap failed\n");
		if (held)
			release_mem_region(pci_vcfg_base, map_len);
		return -ENOMEM;
	}

	b = readb(map);
	pr_info("pci_vcfg_short_read: readb -> 0x%02x\n", b);

	w = readw(map);
	pr_info("pci_vcfg_short_read: readw -> 0x%04x\n", w);

	iounmap(map);
	if (held)
		release_mem_region(pci_vcfg_base, map_len);
	return 0;
}

static void __exit pci_vcfg_short_read_exit(void)
{
	pr_info("pci_vcfg_short_read: unloaded\n");
}

module_init(pci_vcfg_short_read_init);
module_exit(pci_vcfg_short_read_exit);
