SECTIONS
{
  .rodata.gnu_build_id :
  {
    PROVIDE(g_note_build_id = .);
    *(.note.gnu.build-id)
  } > FLASH
} INSERT AFTER .rodata;

SECTIONS
{
  PROVIDE(_flash_start = ORIGIN(APP_FLASH));
  PROVIDE(_flash_end = ORIGIN(APP_FLASH) + LENGTH(APP_FLASH));
  PROVIDE(_ram_start = ORIGIN(APP_RAM));
  PROVIDE(_ram_end = ORIGIN(APP_RAM) + LENGTH(APP_RAM));
}
