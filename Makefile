
build:
	cargo build --release -p picopif --target thumbv6m-none-eabi

run:
	cargo run --release -p picopif --target thumbv6m-none-eabi

debug:
	cargo run -p picopif --target thumbv6m-none-eabi

debug-gdb:
	cargo embed

embed-gdb-remote:
	arm-none-eabi-gdb target/thumbv6m-none-eabi/debug/picopif --eval-command="target remote :2345"
	#arm-none-eabi-gdb target/thumbv6m-none-eabi/debug/picopif -command=.gdbinit --eval-command="target remote :2345"

openocd-defmt:
	nc localhost 7701 | defmt-print -e target/thumbv6m-none-eabi/debug/picopif