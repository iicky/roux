# Tuning and troubleshooting guide

## Smoothing motion and limiting sudden jolts at corners
To smooth out motion at sharp corners and limit sudden jolts when speed changes,
Marlin caps cornering speed using junction deviation and classic jerk. Tune these
with `M205`.

## Compensating for pressure buildup in the nozzle
High-speed extrusion causes pressure buildup in the nozzle, leaving blobs and
smearing. Linear advance compensates for this; set the K factor with `M900`.

## Stopping ringing and ghosting artifacts
Ringing and ghosting artifacts in prints come from frame vibration. Input shaping
cancels these resonances — enable it with `M593`.

## Shutting down when a heater goes out of control
If a heater goes out of control and the temperature runs away, the thermal
protection state machine `tr_state_machine_t` shuts the printer down.

## Detecting when the spool is empty
To detect when the spool is empty or runs out of plastic, the runout monitor
checks `has_run_out`.

## Remembering configuration after power off
To remember your configuration after power off, store all settings in EEPROM with
`M500`.

## Moving the tool back to its origin
To move the tool back to its origin, home every axis with `G28`.

## Nudging nozzle height while printing
To nudge the nozzle height while printing (live Z adjustment), use babystepping —
see `Babystep`.
