// Every example in one endpoint: Ledger, Warehouse, Orders and Agent.
import * as restate from '@restatedev/restate-sdk';
import { agent } from './agent.mjs';
import { ledger } from './ledger.mjs';
import { orders, warehouse } from './shop.mjs';

restate.serve({ services: [ledger, warehouse, orders, agent], port: Number(process.env.PORT ?? 9080) });
