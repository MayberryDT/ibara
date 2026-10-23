# Compositor settings

Use Omarchy’s stock Hyprland desktop for the normal setup. Its Cua plugin checks input against windows, surfaces and popups.

A compositor manages the windows on your desktop. This guide describes an optional input route for Hypoland, which has no plugin system.

## Hypoland input and limits

ibara selects compositor input automatically on Hypoland. This route supports:

- screen capture and accessibility reads
- app launch, clicks and US keyboard text
- shortcuts, scrolling and drags within one window

Other text uses accessibility insertion when the app supports it.

This route has reduced input safeguards. It checks window positions and sizes before input, but cannot enforce the plugin’s surface and popup rules.

It cannot detect a person’s physical keystrokes. Mouse movement stops input between steps. Keyboard commands need a keyboard registered with the compositor.

Doctor and agent status report these limits. Testing used a nested Hypoland desktop. Old GPU hardware and Take Control on Hypoland still need verification.

This option does not extend ibara’s supported platforms beyond Omarchy.

## Select the input route

On stock Hyprland, the plugin is the default. If the plugin is unavailable, you can choose the optional route with its reduced safeguards.

Put this in `~/.config/ibara/settings.toml`:

```toml
[agents]
input_backend = "dispatchers"
```

Use `"auto"` for the normal compositor choice, or `"plugin"` to require the plugin.

If the plugin refuses an input, ibara never retries that input through compositor commands.

Return to the [setup guide](../README.md#before-you-start) or read the [architecture guide](architecture.md).
