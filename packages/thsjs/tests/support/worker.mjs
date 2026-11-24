const { withTestTHS } = await import(process.argv[3] ?? '@zcashlabs/thsjs');

await withTestTHS(async ths => {
  process.send(ths.config());
  await new Promise(() => {});
}, JSON.parse(process.argv[2]));
