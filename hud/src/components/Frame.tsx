import { useId, type ReactNode } from "react";

/**
 * Shared FUI panel chrome (restyle contract item 4): an angular cut-corner
 * frame (octagonal clip-path stroke), four corner brackets, and an optional
 * beveled-trapezoid title bar with a leading light-streak edge plus tick
 * decorations. Purely presentational — all data stays in the children.
 *
 * A11Y: the panel title is a REAL `<h2>` (styled identically via the existing
 * `.t` class + a UA reset in styles.css) and the `<section>` is named by it via
 * `aria-labelledby` — so a screen reader gets ~60 NAMED regions plus
 * heading-based navigation instead of anonymous generic sections. A titleless
 * frame stays an unnamed section (there is genuinely nothing to call it).
 */
export default function Frame({
  className = "",
  title,
  tag,
  children,
}: {
  className?: string;
  title?: ReactNode;
  tag?: ReactNode;
  children: ReactNode;
}) {
  const titleId = useId();
  return (
    <section
      className={`frame ${className}`}
      aria-labelledby={title !== undefined ? titleId : undefined}
    >
      <div className="frame-clip">
        <div className="frame-inner">
          {title !== undefined && (
            <div className="frame-title">
              <h2 className="t" id={titleId}>
                {title}
              </h2>
              {tag !== undefined && <span className="tag">{tag}</span>}
              <span className="ticks" aria-hidden="true">
                <i />
                <i />
                <i />
              </span>
            </div>
          )}
          {children}
        </div>
      </div>
      <i className="bk tl" aria-hidden="true" />
      <i className="bk tr" aria-hidden="true" />
      <i className="bk bl" aria-hidden="true" />
      <i className="bk br" aria-hidden="true" />
    </section>
  );
}
