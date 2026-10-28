import fs from 'node:fs';
import {configuration} from './configure-netlify.mjs';
const file=new URL('./netlify.toml',import.meta.url);
if(!fs.existsSync(file))throw Error('Netlify upstream is not configured');
const raw=fs.readFileSync(file,'utf8');
const target=raw.match(/^  to = "(https:\/\/[^"\n]+)\/api\/:splat"$/m)?.[1];
if(!target||configuration(target)!==raw)throw Error('Exact reviewed API-preserving Netlify configuration required');
