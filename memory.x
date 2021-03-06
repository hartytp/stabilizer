MEMORY
{
  ITCM   (rwx) : ORIGIN = 0x00000000, LENGTH = 64K
  RAM    (rwx) : ORIGIN = 0x20000000, LENGTH = 128K
  RAM_D1 (rwx) : ORIGIN = 0x24000000, LENGTH = 512K
  RAM_D2 (rwx) : ORIGIN = 0x30000000, LENGTH = 288K
  RAM_D3 (rwx) : ORIGIN = 0x38000000, LENGTH = 64K
  RAM_B  (rwx) : ORIGIN = 0x38800000, LENGTH = 4K
  FLASH  (rx)  : ORIGIN = 0x08000000, LENGTH = 1024K
  FLASH1 (rx)  : ORIGIN = 0x08100000, LENGTH = 1024K
}

SECTIONS {
  .sram1 (NOLOAD) : ALIGN(4) {
    *(.sram1);
    . = ALIGN(4);
    } > RAM_D2
} INSERT AFTER .bss;
