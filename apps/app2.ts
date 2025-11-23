const server = Bun.serve({
  port: 3002,
  routes: {
    "/api/status": new Response("OK from app2"),
  },
});

console.log(`Server running at ${server.url}`);
