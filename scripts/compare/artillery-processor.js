// Ends each virtual user's send loop after DURATION seconds, so Artillery
// runs for the same time as the other tools.
const seconds = parseInt(process.env.DURATION || '20', 10);

module.exports = {
  setDeadline(context, events, next) {
    context.vars.deadline = Date.now() + seconds * 1000;
    next();
  },
  beforeDeadline(context, next) {
    next(Date.now() < context.vars.deadline);
  },
};
