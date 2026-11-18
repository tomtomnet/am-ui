# Reference values

The `.json` and `.txt` files here are what the first version of am-ui (Python) computed for the
same inputs; the Rust engine's tests compare their results with them. The `.py` scripts that wrote
them need that version (tag `qt-python-final` of the history before 2026-11-15), so they are kept
as a record of how each file was made, not to be run again. `pipewire_chain.sh` rebuilds the
reference of the downmix chain (`pipewire_chain.c`, PipeWire's own filter code).
