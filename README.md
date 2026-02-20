# MangoChill

Input-based dynamic FPS limiter for Linux. Monitors input devices via evdev and adjusts the FPS limit sent to [MangoHud](https://github.com/flightlessmango/MangoHud) based on user activity.
When you're actively using your mouse, FPS ramps up; when idle, it tapers off to save power.

Read more in the [blog post](https://farnoy.dev/posts/mangochill).
Try the [interactive tuner](https://farnoy.dev/tools/mangochill-tuner) to experiment with parameters using your own mouse.

<video class="video-prose" controls loop autoplay muted playsinline width="817" height="626" src="https://farnoy.dev/assets/mangochill-demo.webm" />


Uses an exponentially weighted envelope follower with asymmetric attack and release half-lives, borrowed from audio signal processing.
The attack/release asymmetry lets FPS ramp up quickly for responsiveness while decaying slowly for smoothness.
Device polling rates are detected automatically so the algorithm behaves consistently regardless of your mouse.

## Architecture

- **server** -- privileged daemon that reads evdev input devices and exposes an RPC interface over a Unix socket. Runs as a dedicated system user with minimal permissions.
- **client** -- unprivileged process that subscribes to FPS limit updates from the server and forwards them to MangoHud's control socket.
- **raw_export** -- utility to stream raw input event timestamps as newline-delimited JSON for debugging.

## Running

```sh
$ nix run github:farnoy/mangochill#mangochill-client -- -vv --min-fps 10 --max-fps 60 --attack-half-life-ms 600 --release-half-life-ms 2000
```

## Development

```sh
$ direnv allow
# or:
$ nix develop
```

With the environment set up, you can now modify & run the binaries directly:

```sh
$ cargo run --bin mangochill-client -- ...
```

## NixOS module

Add the flake as an input to your NixOS configuration:

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    mangochill = {
      url = "github:farnoy/mangochill";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
    };
  };

  outputs = { nixpkgs, mangochill, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        mangochill.nixosModules.default
        {
          services.mangochill.enable = true;
        }
      ];
    };
  };
}
```

This sets up:

- A `mangochill-server` systemd service running as a dedicated system user
- udev rules granting the service read access to input devices
- `mangochill-server`, `mangochill-client`, and `mangochill-raw-export` binaries in `PATH`

The server needs less than 10 MiB of memory and only subscribes to input devices during active sessions. Input events are easy to process but I paid some attention to vectorize the hot loops and made the NixOS module compile using `-C target-cpu=native`.

Run an app through MangoHud, and then:

```sh
$ mangochill-client -vv --min-fps 10 --max-fps 60 --attack-half-life-ms 600 --release-half-life-ms 2000
```


## TODOs / Roadmap

- [ ] Gamepad support
- [ ] Keyboard support
- [ ] Packaging for other distros
