import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import {
  IMAGE_GENERATED_EVENT,
  parseImageGenerated,
  type ImageGenerated,
  type TelemetryEnvelope,
} from "../core/events";
import ImagePanel from "../components/ImagePanel";
import { initialState, reduce, type HudState } from "../core/state";

/* ------------------------------------------------------------------------ *
 * parseImageGenerated (image.generated — the ON-DEVICE IMAGE-GENERATION event).*
 * The daemon runs a Stable-Diffusion-class MLX DIFFUSION model ON-DEVICE for a *
 * "generate/make/draw an image of X" intent and emits this HUD-bound event     *
 * (NEVER over the network) after generate_image returns. CRITICAL: this event   *
 * is METADATA ONLY — NO prompt, NO pixels ever ride telemetry, and the diffusion*
 * seed is intentionally DROPPED. The parser surfaces only {available, path,     *
 * model, size, steps, image}. `available` is true ONLY when the on-device model *
 * actually produced + saved an image; false on every gate/unavailable/transport *
 * fall-back — NEVER a fabricated image, NEVER a cloud fall-back. A malformed     *
 * payload yields null, never a throw.                                           *
 * ------------------------------------------------------------------------ */

describe("parseImageGenerated (image.generated — on-device diffusion, METADATA ONLY)", () => {
  it("parses the available (image saved on-device) outcome with metadata", () => {
    const g = parseImageGenerated({
      available: true,
      path: "/Users/me/darwin/state/images/img-2026.png",
      model: "sdxl-turbo-mlx",
      size: 768,
      steps: 4,
      image: true,
    });
    expect(g).toEqual({
      available: true,
      path: "/Users/me/darwin/state/images/img-2026.png",
      model: "sdxl-turbo-mlx",
      size: 768,
      steps: 4,
      image: true,
    });
  });

  it("parses the honest unavailable outcome (model enabled but produced nothing)", () => {
    const g = parseImageGenerated({ available: false, path: null, model: null, image: true });
    expect(g).toEqual({
      available: false,
      path: null,
      model: null,
      size: null,
      steps: null,
      image: true,
    });
  });

  it("parses the shipped-OFF outcome (model disabled, no image)", () => {
    const g = parseImageGenerated({ available: false, image: false });
    expect(g).toEqual({
      available: false,
      path: null,
      model: null,
      size: null,
      steps: null,
      image: false,
    });
  });

  it("returns null only when the payload is not a plain object", () => {
    expect(parseImageGenerated(null as unknown as Record<string, unknown>)).toBeNull();
    expect(parseImageGenerated("junk" as unknown as Record<string, unknown>)).toBeNull();
    expect(parseImageGenerated([] as unknown as Record<string, unknown>)).toBeNull();
  });

  it("defaults available/image to FALSE when omitted (never a fake 'it generated')", () => {
    const g = parseImageGenerated({});
    expect(g).toEqual({
      available: false,
      path: null,
      model: null,
      size: null,
      steps: null,
      image: false,
    });
  });

  it("downgrades an 'available but no path' payload to NOT available (no phantom file)", () => {
    // An image with nowhere on-device to point at is not a real result.
    const g = parseImageGenerated({ available: true, path: "", model: "m", size: 512, image: true });
    expect(g!.available).toBe(false);
    expect(g!.path).toBeNull();
    // The non-secret metadata still survives the downgrade.
    expect(g!.model).toBe("m");
    expect(g!.size).toBe(512);
  });

  it("never carries a path on an unavailable outcome even if one is smuggled in", () => {
    const g = parseImageGenerated({ available: false, path: "/state/images/x.png", image: true });
    expect(g!.available).toBe(false);
    expect(g!.path).toBeNull();
  });

  it("carries NO prompt / NO pixels / NO seed — only the six metadata fields survive", () => {
    // A hostile payload that tries to smuggle the prompt / pixels / seed through
    // must NOT leak them: the parser surfaces ONLY the contracted fields.
    const g = parseImageGenerated({
      available: true,
      path: "/state/images/x.png",
      model: "sdxl-turbo-mlx",
      size: 768,
      steps: 4,
      image: true,
      // none of these should ever be present, but prove they never round-trip:
      prompt: "a cat astronaut riding a unicorn",
      pixels: [1, 2, 3],
      seed: 1234567,
      image_b64: "AAAA",
    }) as ImageGenerated & Record<string, unknown>;
    expect(Object.keys(g).sort()).toEqual([
      "available",
      "image",
      "model",
      "path",
      "size",
      "steps",
    ]);
    expect(g.prompt).toBeUndefined();
    expect(g.pixels).toBeUndefined();
    expect(g.seed).toBeUndefined();
    expect(g.image_b64).toBeUndefined();
  });

  it("defaults size/steps/model to null when not reported", () => {
    const g = parseImageGenerated({ available: true, path: "/state/images/x.png", image: true });
    expect(g!.model).toBeNull();
    expect(g!.size).toBeNull();
    expect(g!.steps).toBeNull();
  });
});

/* ------------------------------------------------------------------------ *
 * Reducer: image.generated (channel "local") updates state.imageGenerated.     *
 * It is a top-level telemetry envelope (NOT an app.data topic). METADATA ONLY — *
 * nothing visual / no prompt lands in state. Like vision.describe it rides      *
 * channel "local" and is NOT an inference-proof event, so it must NOT clear the *
 * sticky inference-offline banner. There is NEVER a cloud fall-back.            *
 * ------------------------------------------------------------------------ */

function env(event: string, data: Record<string, unknown>): TelemetryEnvelope {
  return { ts: "2026-06-17T12:00:00.000Z", source: "local", event, data };
}

function tel(state: HudState, e: TelemetryEnvelope, at = 1000): HudState {
  return reduce(state, { type: "telemetry", envelope: e, at });
}

function connected(): HudState {
  return reduce(initialState(), { type: "ws.connected", at: 0 });
}

describe("reducer: image.generated (local channel, metadata-only)", () => {
  it("starts null and stores the parsed available outcome", () => {
    let s = connected();
    expect(s.imageGenerated).toBeNull();
    s = tel(
      s,
      env(IMAGE_GENERATED_EVENT, {
        available: true,
        path: "/state/images/x.png",
        model: "sdxl-turbo-mlx",
        size: 768,
        steps: 4,
        image: true,
      }),
    );
    expect(s.imageGenerated).toEqual({
      available: true,
      path: "/state/images/x.png",
      model: "sdxl-turbo-mlx",
      size: 768,
      steps: 4,
      image: true,
    });
  });

  it("stores the honest unavailable outcome (available=false) too", () => {
    let s = connected();
    s = tel(s, env(IMAGE_GENERATED_EVENT, { available: false, image: false }));
    expect(s.imageGenerated).toEqual({
      available: false,
      path: null,
      model: null,
      size: null,
      steps: null,
      image: false,
    });
  });

  it("a newer image.generated replaces the prior one", () => {
    let s = connected();
    s = tel(s, env(IMAGE_GENERATED_EVENT, { available: false, image: true }));
    s = tel(s, env(IMAGE_GENERATED_EVENT, { available: true, path: "/state/images/y.png", image: true }));
    expect(s.imageGenerated!.available).toBe(true);
    expect(s.imageGenerated!.path).toBe("/state/images/y.png");
  });

  it("drops a malformed image.generated without wiping the last posture", () => {
    let s = connected();
    s = tel(s, env(IMAGE_GENERATED_EVENT, { available: true, path: "/state/images/x.png", image: true }));
    const before = s.imageGenerated;
    s = tel(s, { ...env(IMAGE_GENERATED_EVENT, {}), data: "junk" as unknown as Record<string, unknown> });
    expect(s.imageGenerated).toEqual(before);
  });

  it("does NOT clear the sticky inference banner (local-channel, not a proof event)", () => {
    // image.generated rides channel "local"; an offline banner must NOT clear on
    // it (only events that prove the inference server responded clear it).
    let s = connected();
    s = tel(s, { ...env("inference.unavailable", { op: "generate_image", error: "down" }), source: "system" });
    expect(s.inferenceOffline).toBe(true);
    s = tel(s, env(IMAGE_GENERATED_EVENT, { available: false, image: true }));
    expect(s.inferenceOffline).toBe(true);
  });

  it("surfaces a SEPARATE inference.unavailable for the generate_image op on a transport error", () => {
    // The daemon emits inference.unavailable {op:"generate_image"} (system source)
    // on a transport error — distinct from the image.generated outcome event.
    let s = connected();
    s = tel(s, { ...env("inference.unavailable", { op: "generate_image", error: "socket reset" }), source: "system" });
    expect(s.inferenceOffline).toBe(true);
    // No image outcome was produced, so the readout stays null (no phantom image).
    expect(s.imageGenerated).toBeNull();
  });

  it("never lands a prompt / pixels / seed in state even if smuggled in", () => {
    let s = connected();
    s = tel(
      s,
      env(IMAGE_GENERATED_EVENT, {
        available: true,
        path: "/state/images/x.png",
        image: true,
        prompt: "SECRET prompt",
        pixels: [1, 2, 3],
        seed: 99,
      }),
    );
    const ig = s.imageGenerated as unknown as Record<string, unknown>;
    expect(ig.prompt).toBeUndefined();
    expect(ig.pixels).toBeUndefined();
    expect(ig.seed).toBeUndefined();
  });
});

/* ------------------------------------------------------------------------ *
 * ImagePanel — the GENERATED-IMAGE readout. HONEST copy: on-device MLX          *
 * diffusion; the prompt + image STAY on the machine, nothing goes to the cloud; *
 * device-gated on a multi-GB model + RAM; OFF/opt-in; absent model => honest    *
 * unavailable, never a fabricated image. The pixels + prompt NEVER appear here. *
 * ------------------------------------------------------------------------ */

function renderPanel(generated: ImageGenerated | null): string {
  return renderToStaticMarkup(createElement(ImagePanel, { generated }));
}

describe("ImagePanel GENERATED-IMAGE readout (on-device diffusion, honest)", () => {
  it("renders the IDLE placeholder before any image.generated arrives", () => {
    const html = renderPanel(null);
    expect(html).toContain("NO IMAGE YET");
    expect(html).not.toContain("GENERATED IMAGE");
    // The honest invite to generate.
    expect(html.toLowerCase()).toContain("draw an image of");
  });

  it("surfaces the GENERATED-IMAGE readout once an outcome arrives (daemon-driven)", () => {
    const html = renderPanel({
      available: true,
      path: "/Users/me/state/images/x.png",
      model: "sdxl-turbo-mlx",
      size: 768,
      steps: 4,
      image: true,
    });
    expect(html).toContain("GENERATED IMAGE");
    expect(html).not.toContain("NO IMAGE YET");
  });

  it("shows WHERE the image landed on-device (the local path) + non-secret metadata", () => {
    const html = renderPanel({
      available: true,
      path: "/Users/me/state/images/x.png",
      model: "sdxl-turbo-mlx",
      size: 768,
      steps: 4,
      image: true,
    });
    expect(html).toContain("ON-DEVICE MLX DIFFUSION");
    expect(html).toContain("SAVED");
    expect(html).toContain("ON-DEVICE PATH");
    expect(html).toContain("/Users/me/state/images/x.png");
    expect(html).toContain("sdxl-turbo-mlx");
    expect(html).toContain("768px");
    expect(html).toContain("4 STEPS");
  });

  it("NEVER renders the pixels or the prompt on the available readout — honest copy", () => {
    const html = renderPanel({
      available: true,
      path: "/Users/me/state/images/x.png",
      model: "sdxl-turbo-mlx",
      size: 768,
      steps: 4,
      image: true,
    });
    expect(html.toLowerCase()).toContain("the prompt stays on the machine");
    expect(html.toLowerCase()).toContain("pixels never ride this readout");
    expect(html.toLowerCase()).toContain("nothing goes to the cloud");
    expect(html.toLowerCase()).toContain("device-gated");
    expect(html.toLowerCase()).toContain("off/opt-in");
    expect(html).toContain("MODEL ON");
  });

  it("shows an honest unavailable state when the model is enabled but produced nothing", () => {
    const html = renderPanel({
      available: false,
      path: null,
      model: null,
      size: null,
      steps: null,
      image: true,
    });
    expect(html).toContain("UNAVAILABLE");
    expect(html.toLowerCase()).toContain("failed honestly");
    expect(html.toLowerCase()).toContain("no image is invented");
    expect(html.toLowerCase()).toContain("no cloud fall-back");
    expect(html).toContain("MODEL ON");
    expect(html).not.toContain("ON-DEVICE PATH");
    expect(html).not.toContain("SAVED");
  });

  it("shows an honest 'model not set up / needs download' state when image gen ships OFF", () => {
    const html = renderPanel({
      available: false,
      path: null,
      model: null,
      size: null,
      steps: null,
      image: false,
    });
    expect(html).toContain("UNAVAILABLE");
    expect(html.toLowerCase()).toContain("on-device image model isn&#x27;t set up");
    expect(html.toLowerCase()).toContain("multi-gb mlx diffusion model download");
    expect(html.toLowerCase()).toContain("ships off/opt-in");
    expect(html).toContain("MODEL OFF");
    expect(html).not.toContain("ON-DEVICE PATH");
  });

  it("never renders a path on an unavailable outcome (no phantom file)", () => {
    // Even if a path somehow accompanies an unavailable outcome, the parser would
    // have nulled it; the panel must not render an ON-DEVICE PATH row.
    const html = renderPanel({
      available: false,
      path: null,
      model: "sdxl-turbo-mlx",
      size: null,
      steps: null,
      image: true,
    });
    expect(html).not.toContain("ON-DEVICE PATH");
  });
});
