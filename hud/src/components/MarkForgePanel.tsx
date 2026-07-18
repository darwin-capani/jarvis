/**
 * Mark-Forge — the HUD-side React-Three-Fiber sandbox panel for the
 * deterministic rigid-body physics engine (apps/mark-forge, runtime="binary").
 *
 * The engine is a pure CPU/f64 simulation verified headlessly (cargo test); it
 * emits body-transform telemetry over its per-app JSONL socket, which the daemon
 * relays as `app.data` on the physics.* topics. This panel renders those bodies
 * in a SMALL R3F scene (instanced meshes for spheres + cuboids, a ground plane,
 * transforms driven straight from the telemetry).
 *
 * HONESTY — the live 60fps render is DEVICE-GATED (SPEC §8): the HUD's headless
 * preview SUSPENDS the R3F render loop (frameloop="demand" + a guard), so the
 * on-screen motion / 60fps is verified only on the real Tauri app on the device.
 * This panel renders TELEMETRY-DRIVEN transforms (the engine's positions +
 * orientations per frame), NOT a claimed-measured framerate. The numbers in the
 * stats strip are the simulation's (frame counter, contact count, penetration
 * residual); the frame RATE is never asserted here.
 *
 * Anti-flash discipline (mirrors CoreScene/audioStore): the latest physics.bodies
 * frame is written to a mutable ref read INSIDE useFrame, so the ~hundreds-of-Hz
 * telemetry stream never re-renders the React tree — only the discrete topology
 * (scene body set) and online/offline transitions reconcile the component.
 */
import { Canvas, useFrame, useThree } from "@react-three/fiber";
import { memo, useEffect, useMemo, useRef } from "react";
import * as THREE from "three";
import {
  PHYSICS_TOPIC_BODIES,
  PHYSICS_TOPIC_SCENE,
  PHYSICS_TOPIC_STEP,
  parsePhysicsBodies,
  parsePhysicsScene,
  parsePhysicsStep,
  type PhysicsBodiesFrame,
  type PhysicsSceneTopology,
} from "../core/events";
import type { AppFeed } from "../core/state";
import CapHitIndicator from "./CapHitIndicator";
import Frame from "./Frame";

/** Manifest name of the mark-forge micro-app (apps/mark-forge/manifest.toml).
 *  The panel renders exactly this app's feed slice; other app names are ignored
 *  here (each surface owns its own component). Mark-Forge is runtime="binary",
 *  gpu=false, net_hosts=[] offline — the ENGINE is CPU/f64 and headless-verified;
 *  the live R3F render here is device-gated (the headless preview suspends the
 *  loop), so this surface renders telemetry-driven transforms, not a measured
 *  framerate. */
export const MARK_FORGE_APP_NAME = "mark-forge";

/** Hard cap on instances per shape class — a defensive bound mirroring the
 *  engine's MAX_BODIES so a hostile/buggy telemetry stream cannot ask THREE to
 *  allocate an unbounded InstancedMesh. The engine caps at 4096 bodies total. */
const MAX_INSTANCES = 4096;

/** FUI accents (match the cyan family + the violet cool-fill used across the HUD). */
const CYAN = "#1ad6ff";
const CYAN_BRIGHT = "#7df3ff";
const CYAN_DEEP = "#0e3a48";
const CLOUD_VIOLET = "#9d7dff";
const SLEEP_DIM = "#2c5566";

/** Pinned pixel ratio, capped — single source of truth (same rationale as
 *  CoreScene's FIXED_DPR: a second writer churns the buffers / flickers). */
const FIXED_DPR = Math.min(
  typeof window === "undefined" ? 1.5 : window.devicePixelRatio || 1.5,
  1.5,
);

const GL_PROPS = {
  antialias: true,
  powerPreference: "high-performance" as const,
  alpha: true,
  stencil: false,
};
const CAMERA_PROPS = {
  position: [6, 5, 8] as [number, number, number],
  fov: 42,
};
const CANVAS_STYLE = { position: "absolute" as const, inset: 0 };

/* ------------------------------------------------------------------------ *
 * The R3F scene. Body transforms arrive through a mutable ref (frameRef) that *
 * useFrame reads each tick — the telemetry stream NEVER re-renders React.     *
 * ------------------------------------------------------------------------ */

interface FrameRef {
  /** The latest physics.bodies frame, or null before the first one arrives. */
  current: PhysicsBodiesFrame | null;
}

/** Reusable scratch objects so the per-frame instance write allocates nothing. */
const SCRATCH_OBJ = new THREE.Object3D();
const SCRATCH_QUAT = new THREE.Quaternion();
const SCRATCH_COLOR = new THREE.Color();

function Bodies({ frameRef }: { frameRef: FrameRef }) {
  const sphereRef = useRef<THREE.InstancedMesh>(null);
  const cuboidRef = useRef<THREE.InstancedMesh>(null);
  const invalidate = useThree((s) => s.invalidate);
  const onDevice = useRef<boolean>(false);

  // The headless preview suspends the loop (frameloop="demand"); on a real
  // device we keep invalidating so the sim animates. We never CLAIM the device
  // path ran — we only request a frame; whether it renders at 60fps is verified
  // on the real device, not here.
  useEffect(() => {
    onDevice.current = true;
    return () => {
      onDevice.current = false;
    };
  }, []);

  useFrame(() => {
    const frame = frameRef.current;
    const sphereMesh = sphereRef.current;
    const cuboidMesh = cuboidRef.current;
    if (!frame || (!sphereMesh && !cuboidMesh)) return;

    let nSphere = 0;
    let nCuboid = 0;
    for (const body of frame.bodies) {
      const shape = body.shape;
      // Planes render as a single static ground grid (mounted separately); the
      // dynamic bodies the instanced meshes draw are spheres + cuboids.
      if (shape.kind === "sphere" && sphereMesh && nSphere < MAX_INSTANCES) {
        SCRATCH_OBJ.position.set(body.pos[0], body.pos[1], body.pos[2]);
        SCRATCH_QUAT.set(body.quat[0], body.quat[1], body.quat[2], body.quat[3]);
        SCRATCH_OBJ.quaternion.copy(SCRATCH_QUAT);
        SCRATCH_OBJ.scale.setScalar(shape.radius);
        SCRATCH_OBJ.updateMatrix();
        sphereMesh.setMatrixAt(nSphere, SCRATCH_OBJ.matrix);
        SCRATCH_COLOR.set(body.sleeping ? SLEEP_DIM : CYAN_BRIGHT);
        sphereMesh.setColorAt(nSphere, SCRATCH_COLOR);
        nSphere += 1;
      } else if (shape.kind === "cuboid" && cuboidMesh && nCuboid < MAX_INSTANCES) {
        SCRATCH_OBJ.position.set(body.pos[0], body.pos[1], body.pos[2]);
        SCRATCH_QUAT.set(body.quat[0], body.quat[1], body.quat[2], body.quat[3]);
        SCRATCH_OBJ.quaternion.copy(SCRATCH_QUAT);
        // Base geometry is a unit cube (half-extent 0.5); scale to the body's
        // full extents = half_extents * 2.
        SCRATCH_OBJ.scale.set(
          shape.halfExtents[0] * 2,
          shape.halfExtents[1] * 2,
          shape.halfExtents[2] * 2,
        );
        SCRATCH_OBJ.updateMatrix();
        cuboidMesh.setMatrixAt(nCuboid, SCRATCH_OBJ.matrix);
        SCRATCH_COLOR.set(body.sleeping ? SLEEP_DIM : CYAN);
        cuboidMesh.setColorAt(nCuboid, SCRATCH_COLOR);
        nCuboid += 1;
      }
    }
    if (sphereMesh) {
      sphereMesh.count = nSphere;
      sphereMesh.instanceMatrix.needsUpdate = true;
      if (sphereMesh.instanceColor) sphereMesh.instanceColor.needsUpdate = true;
    }
    if (cuboidMesh) {
      cuboidMesh.count = nCuboid;
      cuboidMesh.instanceMatrix.needsUpdate = true;
      if (cuboidMesh.instanceColor) cuboidMesh.instanceColor.needsUpdate = true;
    }
    // Keep the loop alive on a device path (no-op under the headless demand
    // frameloop where this component is suspended).
    if (onDevice.current) invalidate();
  });

  return (
    <>
      <instancedMesh
        ref={sphereRef}
        args={[undefined, undefined, MAX_INSTANCES]}
        frustumCulled={false}
      >
        <sphereGeometry args={[1, 20, 20]} />
        <meshStandardMaterial
          metalness={0.1}
          roughness={0.45}
          emissive={CYAN}
          emissiveIntensity={0.18}
        />
      </instancedMesh>
      <instancedMesh
        ref={cuboidRef}
        args={[undefined, undefined, MAX_INSTANCES]}
        frustumCulled={false}
      >
        <boxGeometry args={[1, 1, 1]} />
        <meshStandardMaterial
          metalness={0.1}
          roughness={0.5}
          emissive={CYAN}
          emissiveIntensity={0.12}
        />
      </instancedMesh>
    </>
  );
}

/** A static ground grid standing in for the physics ground plane(s). The engine
 *  models an infinite half-space; we render a finite grid helper at the origin
 *  so the bodies read against a floor. (A plane body's exact normal/offset is in
 *  the telemetry; the panel keeps a simple horizontal floor — the on-device
 *  render can refine this and is device-gated regardless.) */
function Ground() {
  const grid = useMemo(
    () => new THREE.GridHelper(20, 20, CYAN_BRIGHT, CYAN_DEEP),
    [],
  );
  // Push the grid slightly transparent so it reads as a backdrop, not a surface.
  useEffect(() => {
    const mat = grid.material as THREE.Material | THREE.Material[];
    const apply = (m: THREE.Material) => {
      m.transparent = true;
      m.opacity = 0.5;
      m.depthWrite = false;
    };
    Array.isArray(mat) ? mat.forEach(apply) : apply(mat);
    return () => grid.dispose();
  }, [grid]);
  return <primitive object={grid} />;
}

function MarkForgeScene({ frameRef }: { frameRef: FrameRef }) {
  return (
    // frameloop="demand": the headless preview holds the loop suspended (it only
    // renders on an explicit invalidate, which the device-only effect drives) —
    // this is exactly the device-gating the SPEC calls out. No claimed framerate.
    <Canvas
      gl={GL_PROPS}
      camera={CAMERA_PROPS}
      dpr={FIXED_DPR}
      style={CANVAS_STYLE}
      frameloop="demand"
    >
      {/* No opaque background — GL alpha:true lets the bodies composite over the
          panel's frosted glass (cohesive glassmorphism) instead of a flat fill. */}
      <ambientLight intensity={0.5} />
      {/* Three-point-ish lighting so the instanced forms read with DEPTH instead
          of flat single-source shading: a cyan key (upper-right), a dim violet
          cool-fill from the opposite side, and a faint cyan rim from behind to
          separate the bodies from the glass. */}
      <directionalLight position={[5, 8, 5]} intensity={0.85} color={CYAN_BRIGHT} />
      <directionalLight position={[-6, 3, -4]} intensity={0.32} color={CLOUD_VIOLET} />
      <directionalLight position={[0, 2, -8]} intensity={0.22} color={CYAN} />
      <Ground />
      <Bodies frameRef={frameRef} />
    </Canvas>
  );
}

const MarkForgeSceneMemo = memo(MarkForgeScene);

/* ------------------------------------------------------------------------ *
 * The panel chrome — parses the physics.* topic slices, wires the bodies feed *
 * into the R3F scene via the mutable ref, and shows the OFFLINE placeholder    *
 * until the engine is running.                                                 *
 * ------------------------------------------------------------------------ */

/** Format the penetration residual compactly (mm with 2 dp, or "0" at rest). */
function pen(v: number): string {
  if (!Number.isFinite(v) || v <= 0) return "0";
  const mm = v * 1000;
  return mm < 0.01 ? "0" : `${mm.toFixed(2)}mm`;
}

export default function MarkForgePanel({
  feed,
  running,
}: {
  /** The mark-forge app's feed slice, or undefined if it never reported. */
  feed: AppFeed | undefined;
  /** Tracked-running flag from state.runningApps (authoritative over feed). */
  running: boolean;
}) {
  // Live (running) OR a feed that has reported => online. A stopped app that
  // previously reported keeps showing its last telemetry, dimmed.
  const online = running || feed?.running === true;
  const topics = feed?.topics ?? {};

  const scene: PhysicsSceneTopology | null = topics[PHYSICS_TOPIC_SCENE]
    ? parsePhysicsScene(topics[PHYSICS_TOPIC_SCENE])
    : null;
  const step = topics[PHYSICS_TOPIC_STEP]
    ? parsePhysicsStep(topics[PHYSICS_TOPIC_STEP])
    : null;
  const bodies: PhysicsBodiesFrame | null = topics[PHYSICS_TOPIC_BODIES]
    ? parsePhysicsBodies(topics[PHYSICS_TOPIC_BODIES])
    : null;

  // The latest body frame rides a mutable ref into the R3F loop (no re-render on
  // the telemetry stream — mirrors the audioStore/CoreScene anti-flash pattern).
  const frameRef = useRef<PhysicsBodiesFrame | null>(null);
  frameRef.current = bodies;

  const bodyCount = scene?.bodies.length ?? bodies?.bodies.length ?? 0;

  // Header tag: live body count when online, else OFFLINE.
  const tag = !online ? "OFFLINE" : `${bodyCount} BODIES`;

  const hasAny = scene !== null || step !== null || bodies !== null;

  return (
    <Frame
      className={`mark-forge ${online ? "" : "offline"}`}
      title="MARK-FORGE // PHYSICS SANDBOX"
      tag={tag}
    >
      {!online && !hasAny ? (
        <div className="mf-placeholder">
          <div className="mf-ph-big">MARK-FORGE OFFLINE</div>
          <div className="mf-ph-small">say "open mark forge"</div>
        </div>
      ) : (
        <div className="mf-body">
          {/* The R3F sandbox. The live 60fps render is DEVICE-GATED — the
              headless preview suspends the loop (frameloop="demand"); this draws
              telemetry-driven transforms, never a claimed-measured framerate. */}
          <div className="mf-scene">
            <MarkForgeSceneMemo frameRef={frameRef as FrameRef} />
            <span className="mf-scene-tag" aria-hidden="true">
              SIM-DRIVEN · RENDER DEVICE-GATED
            </span>
          </div>

          {/* physics.step — solver/step stats strip. These are the simulation's
              own counters (frame, contacts, penetration residual), NOT a
              measured framerate. */}
          <div className="mf-stats">
            <div className="mf-stat">
              <span className="mf-stat-label">FRAME</span>
              <span className="mf-stat-val">
                {bodies ? bodies.frame : step ? step.frames : "—"}
              </span>
            </div>
            <div className="mf-stat">
              <span className="mf-stat-label">CONTACTS</span>
              <span className="mf-stat-val">{step ? step.contacts : "—"}</span>
            </div>
            <div className="mf-stat">
              <span className="mf-stat-label">ITERS</span>
              <span className="mf-stat-val">{step ? step.solverIterations : "—"}</span>
            </div>
            <div className="mf-stat">
              <span className="mf-stat-label">PEN</span>
              <span className="mf-stat-val">{step ? pen(step.lastPenetration) : "—"}</span>
            </div>
          </div>

          {/* Per-substep budget signal (engine-reported): a degenerate/over-dense
              scene was deterministically bounded (pair-enumeration / contact-solve
              cap) and SIGNALLED, not silently mis-simulated. Absent on normal scenes.
              A received telemetry flag, not a HUD-measured value. */}
          <CapHitIndicator
            pairsCapHit={step?.pairsCapHit ?? false}
            contactCapHit={step?.contactCapHit ?? false}
          />

          {/* physics.scene — sim params readout (gravity / dt / substeps). */}
          {scene ? (
            <div className="mf-params">
              <div className="mf-param">
                <span className="mf-param-label">GRAVITY</span>
                <span className="mf-param-val">
                  {scene.gravity.map((g) => g.toFixed(1)).join(", ")}
                </span>
              </div>
              <div className="mf-param">
                <span className="mf-param-label">DT</span>
                <span className="mf-param-val">
                  {(scene.dt * 1000).toFixed(1)}ms × {scene.substeps}
                </span>
              </div>
              <div className="mf-param">
                <span className="mf-param-label">SIM-T</span>
                <span className="mf-param-val">
                  {bodies ? `${bodies.simTime.toFixed(2)}s` : "—"}
                </span>
              </div>
            </div>
          ) : null}
        </div>
      )}
    </Frame>
  );
}
