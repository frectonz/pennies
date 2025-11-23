const server = Bun.serve({
  port: 3001,
  routes: {
    "/api/status": new Response("OK from app1"),
  },
});

console.log(`Server running at ${server.url}`);
