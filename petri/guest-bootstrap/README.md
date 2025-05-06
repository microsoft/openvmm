This directory contains files used to bootstrap the guest with the requested
configuration.

* `meta-data` and `user-data`: cloud-init files for enabling pipette for Linux guests
* `imc-pipette.hiv`: an IMC hive for enabling pipette for Windows guests
* `imc-vsm.hiv`: an IMC hive for enabling VSM for Windows guests

To update an IMC hive file, on a Windows machine run 
`cargo run -p make_imc_hive <type> PATH/TO/imc.hiv`
