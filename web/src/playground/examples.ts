import type { VirtualFile } from './protocol';
import type { PlaygroundMessages } from './messages';
export interface Example {
  id: keyof PlaygroundMessages['examples'];
  source: string;
  files?: VirtualFile[];
}
export const examples: Example[] = [
  {
    id: 'hello',
    source: 'fn main() {\n    println("Hello Ku")\n}\n',
  },
  {
    id: 'arrays',
    source:
      'fn main() {\n    nums:[int] = [1, 2, 3]\n    names:[str] = ["Ku", "Lang"]\n    println(nums[0])\n    println(len(nums))\n    println(names[1])\n}\n',
  },
  {
    id: 'json',
    source:
      'import json from "std.json"\n\nfn main(): null! {\n    message = { language: "Ku", hello: "中😀", count: 9007199254740993 }\n    text = json.encode(message)?\n    println(text)\n    return ok(null)\n}\n',
  },
  {
    id: 'result',
    source:
      'fn double(value: int): int! {\n    return ok(value * 2)\n}\n\nfn main(): null! {\n    value = double(21)?\n    println(value)\n    return ok(null)\n}\n',
  },
  {
    id: 'closure',
    source:
      'fn main() {\n    prefix = "Hello "\n    greet = (name: str) => {\n        return prefix + name\n    }\n    println(greet("Ku"))\n}\n',
  },
  {
    id: 'module',
    source:
      'import math from "./math.ku"\n\nfn main() {\n    println(math.Add(20, 22))\n}\n',
    files: [
      {
        path: 'math.ku',
        source: 'fn Add(a: int, b: int): int {\n    return a + b\n}\n',
      },
    ],
  },
  {
    id: 'error',
    source: 'fn main() {\n    println(missing_name)\n}\n',
  },
];
