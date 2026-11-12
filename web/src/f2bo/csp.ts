import { z } from "zod";

// Use the interpreter under the production CSP instead of probing dynamic code generation.
z.config({ jitless: true });
