import type { AuthEnv } from "./auth";
import type { BusLog } from "./do-buslog";
import type { ClaimDO } from "./do-claim";

/** Shared Worker environment/bindings type. Kept in its own module (rather
 * than declared inline in `index.ts`) so the Durable Object classes can
 * reference it for their Workers-RPC base class generic parameter
 * (`DurableObject<Env>`) without a circular import against `index.ts`. */
export interface Env extends AuthEnv {
  BUS_LOG: DurableObjectNamespace<BusLog>;
  CLAIM_DO: DurableObjectNamespace<ClaimDO>;
  /** Free-form identity string surfaced in `/health.hub_identity`. Defaults to `"cloud"`. */
  HUB_IDENTITY?: string;
}
