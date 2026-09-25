# Long press, CGContext paths and software Core Image

## Added regression tests

With the project's Rust build prerequisites installed:

```
cargo test --lib path_geometry::tests
cargo test --lib core_image::pipeline::tests
cargo test --lib long_press_tests
```

These check winding/even-odd holes, curve subdivision, nonrectangular fill,
Gaussian impulse response and extent growth, colour operations, image row
orientation, and long-press defaults/movement threshold. They do not replace
an on-device/guest-application test of Objective-C dispatch and timer delivery.

## Guest runtime checks still required

* Hold on a button for at least 0.5 seconds: the recognizer sends Began before
  finger-up, then Changed on movement and Ended on release. Releasing early
  or moving more than 10 points before recognition must not send an action.
* With cancelsTouchesInView enabled, UIControl receives TouchCancel and clears
  tracking/highlight; it must not subsequently send TouchUpInside. With it
  disabled, ordinary touch delivery continues.
* Disable/detach an active recognizer; its timer must stop and the recognizer
  must report Cancelled. Attach to a parent view and press a child. Exercise
  multiple required fingers, preceding taps, and removal during callbacks.
* Add a CGPath containing lines, curves and multiple subpaths to a transformed
  context, mutate/release the source path, then draw. Check that AddPath copied
  it, fill/stroke share the same geometry, and a second draw sees an empty path.
  Check butt/round/square caps and miter/round/bevel joins under a sheared CTM.
* Construct CIImage from CGImage, release CGImage, run each supported filter,
  drain an autorelease pool with retained inputs/filter/output, and export via
  CIContext. Check crop orientation, alpha and chained blur extents.

## Deliberate coverage limits

The software Core Image filter set is **CIColorControls, CIColorInvert,
CIColorMonochrome, CISepiaTone and CIGaussianBlur**. Unknown filters return nil,
not the original image. Gaussian blur uses a finite 3-sigma separable kernel;
allocation/workload limits return nil with diagnostics. The working space is
linear RGB; export is premultiplied sRGB using the existing Image conversions.
Context options for other colour spaces or unpremultiplied export are rejected,
not silently ignored. This is not full Apple's Core Image compatibility.

Path rendering uses adaptive tessellation and four coverage samples per pixel.
This change does not implement the context's existing unsupported dash,
arbitrary clipping-path, shadow or general blend-mode facilities.

UIButton selectors received by UIView/UIImageView are a separate, unresolved
object-identity/construction issue. UIButton already implements those APIs.
Source inspection of its factory/initializers and NIB decoding did not establish
the origin of the wrong receivers. No UIButton methods were added to unrelated
classes. Reproducing the original application is necessary to verify that issue;
Chrome compatibility is not established by these unit tests.
